//! Every receipt comes from a real authenticated provider POST and terminal barrier.
use super::*;
use crate::tenancy::{command::Command, store::StoredResponse};
use bitrouter_ai::tenancy::authority::{
    BindingPut, GrantScope, TenantPut, TenantRole, TenantState,
};

async fn second(first: &Fixture) -> Result<Fixture> {
    let tenant: bitrouter_ai::tenancy::identity::TenantId =
        first.tenant.as_str().to_ascii_uppercase().try_into()?;
    first
        .service
        .execute(
            first.operator.clone(),
            Command::Tenant(
                tenant.clone(),
                TenantPut {
                    operation_id: "archive-upper-tenant".try_into()?,
                    expected_revision: None,
                    state: TenantState::Active,
                    installation_id: first.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    let binding_result = match first
        .service
        .execute(
            first.operator.clone(),
            Command::Binding(
                tenant.clone(),
                first.binding.clone(),
                BindingPut {
                    operation_id: "archive-upper-binding".try_into()?,
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
        .map_err(failure)?
    {
        StoredResponse::Operation(value) => value,
        _ => anyhow::bail!("expected upper-case binding operation"),
    };
    Ok(Fixture {
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

pub(super) async fn source(
    directory: &Path,
) -> Result<(std::path::PathBuf, Vec<ExecutionReceipt>)> {
    let live = directory.join("live.sqlite");
    let owner = crate::tenancy::ownership::acquire_path(&live, true)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    let mut initial = Fixture::with_database(db.clone(), Default::default(), &mut |_| {}).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let (received, mut observations) = mpsc::channel(16);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let upstream = format!("http://{}/v1", listener.local_addr()?);
    let router = Router::new()
        .route("/v1/chat/completions", post(provider))
        .with_state(Reply {
            status: StatusCode::OK,
            content_type: "application/json",
            body: serde_json::to_string(&json_response(
                "chat_completions",
                full_usage("chat_completions"),
            )?)?,
            calls: calls.clone(),
            received,
            gate: None,
        });
    let mut provider_task = tokio::spawn(async move { axum::serve(listener, router).await });
    let mut gateway_task = None;
    let checked = async {
        // Preserve the original file Lease through all test-only prepared Services.
        // This fixture does not claim native startup/recovery provenance.
        let (fixture, route, slot) = super::super::super::super::provision_protocol_with_fixture(
            Fixture { service: initial.service.clone(), db: db.clone(), operator: initial.operator.clone(),
                installation: initial.installation.clone(), tenant: initial.tenant.clone(),
                principal: initial.principal.clone(), binding: initial.binding.clone(), binding_result: initial.binding_result.clone() },
            &upstream, "chat_completions", &mut |_| {},
        ).await?;
        initial = fixture;
        initial.service.shutdown().await?;
        let mut upper_route = route.clone();
        let mut upper_slot = slot.clone();
        let upper_id = initial.tenant.as_str().to_ascii_uppercase();
        upper_route.owner_id = upper_id.as_str().try_into()?;
        upper_slot.owner_id = upper_id.as_str().try_into()?;
        initial.service.resources.provision_bytes(&db, &serde_json::to_vec(&json!({
            "format":1,"installation_id":initial.installation,"resources":[
                {"reference":upper_slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}},
                {"reference":upper_route,"definition":{"kind":"route","selector":"mock:model-a","policy":null,
                    "routing":{"providers":{"mock":{"api_base":upstream,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"}]}}},
                    "credentials":[{"provider":"mock","account":"default","credential":upper_slot}]}}
            ]
        }))?).await?;
        initial.service = crate::tenancy::Service::start_prepared(
            db.clone(), Some(owner.clone()), Default::default(),
            crate::tenancy::store::load(&db, chrono::Utc::now()).await?, initial.service.resources.clone(),
        ).await?;
        let upper = second(&initial).await?;
        let lower_revision = draft(&initial, &route, "archive-lower-config", 25).await?;
        let upper_revision = draft(&upper, &upper_route, "archive-upper-config", 25).await?;
        let lower_key = initial.issue("archive-lower-key").await?;
        let upper_key = upper.issue("archive-upper-key").await?;
        initial.service.dispatch_test_ready.store(true, Ordering::Release);
        let router = Router::new().route("/v1/chat/completions", post(|| async {StatusCode::NOT_FOUND}))
            .layer(axum::middleware::from_fn_with_state(initial.service.clone(), crate::tenancy::http::dispatch));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/v1/chat/completions", listener.local_addr()?);
        gateway_task = Some(tokio::spawn(async move {axum::serve(listener, router).await}));
        let client = Harness::client(false)?;
        let mut receipts = Vec::new();
        for (fixture, revision, credential, correlation) in [
            (&initial, &lower_revision, &lower_key, "case"),
            (&initial, &lower_revision, &lower_key, "case"),
            (&initial, &lower_revision, &lower_key, "CASE"),
            (&upper, &upper_revision, &upper_key, "case"),
        ] {
            let mut bound = headers(fixture, credential.secret.as_deref().context("credential")?, &revision.revision_id)?;
            bound.insert("bitrouter-correlation-id", correlation.parse()?);
            let response = client.post(&endpoint).headers(bound).json(&json!({
                "model":MODEL,"messages":[{"role":"user","content":"actual archive receipt"}],"stream":false
            })).send().await?;
            ensure!(response.status() == StatusCode::OK);
            let id = execution_id(&response)?;
            response.bytes().await?;
            let observation = tokio::time::timeout(TIMEOUT, observations.recv()).await?.context("actual provider observation")?;
            ensure!(observation["model"] == "model-a");
            let bytes = fixture.service.read_receipt(fixture.operator.clone(), fixture.tenant.clone(), read::Request::One(id.clone())).await.map_err(failure)?;
            let receipt: ExecutionReceipt = serde_json::from_slice(&bytes)?;
            ensure!(receipt.execution_id == id && receipt.tenant_id == fixture.tenant && receipt.correlation_id.as_str() == correlation);
            ensure!(receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed && receipt.attempts.len() == 1);
            ensure!(receipt.attempts[0].outcome == bitrouter_ai::tenancy::execution::Outcome::Completed);
            receipts.push(receipt);
        }
        ensure!(calls.load(Ordering::SeqCst) == 4);
        initial.service.shutdown().await?;
        crate::tenancy::audit::tests::assert_file_database(&db, &live).await?;
        let retired = directory.join("retired.sqlite");
        copy_database(&db, &retired).await?;
        Ok::<_, anyhow::Error>((retired, receipts))
    }.await;
    let stopped = initial.service.shutdown().await;
    let gateway_stopped = if let Some(mut gateway) = gateway_task {
        stop_socket(&mut gateway).await
    } else {
        Ok(())
    };
    let provider_stopped = stop_socket(&mut provider_task).await;
    drop(initial);
    let closed = db.close().await;
    drop(owner);
    // Actual reacquisition, not a delay or a synthetic ownership witness.
    let released = reacquire(&live).await;
    stopped?;
    gateway_stopped?;
    provider_stopped?;
    closed?;
    released?;
    checked
}

pub(super) async fn stop_socket(
    task: &mut tokio::task::JoinHandle<std::io::Result<()>>,
) -> Result<()> {
    task.abort();
    match tokio::time::timeout(TIMEOUT, task).await? {
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(error.into()),
        Ok(result) => result.map_err(Into::into),
    }
}
