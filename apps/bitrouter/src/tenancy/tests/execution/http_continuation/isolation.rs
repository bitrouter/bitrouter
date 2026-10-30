use super::super::super::{expiry, result as applied};
use super::*;
use crate::tenancy::{Service, command::Command, resources::Resources};
use bitrouter_ai::tenancy::{
    authority::{BindingPut, ResourceKind, TenantPut, TenantRole, TenantState},
    configuration::ResourceRef,
};

fn route_record(reference: &ResourceRef, slot: &ResourceRef, url: &str) -> Value {
    json!({"reference":reference,"definition":{"kind":"route","selector":"bitrouter/owned","policy":null,
        "routing":{"providers":{"mock":{"api_base":url,"api_protocol":[{"*":"responses"}],"models":[{"id":"model-a"}]}},"routers":{"owned":{"selection":{"kind":"model","model":"mock:model-a"}}}},
        "credentials":[{"provider":"mock","account":"default","credential":slot}]}})
}

#[tokio::test]
async fn another_installation_cannot_resume_with_equal_names_and_wrapping_key() -> Result<()> {
    let mut provider = Server::start().await?;
    let first = ready(&provider).await?;
    let second = ready(&provider).await?;
    let outcome: Result<()> = async {
        ensure!(first.fixture.installation != second.fixture.installation);
        ensure!(first.fixture.tenant == second.fixture.tenant);
        ensure!(first.fixture.principal == second.fixture.principal);
        ensure!(first.route == second.route);
        let (status, bytes) = send(&first, first.headers.clone(), body(false, None)).await?;
        ensure!(status == StatusCode::OK);
        let public = public_id(&bytes, false)?;
        provider.wire().await?;
        let (status, _) = send(&second, second.headers.clone(), body(false, Some(&public))).await?;
        ensure!(!status.is_success());
        ensure!(provider.state.calls.load(Ordering::SeqCst) == 1);
        ensure!(row_count(&second).await? == 0);
        ensure!(second.fixture.service.current().is_ok());
        let (status, bytes) =
            send(&first, first.headers.clone(), body(true, Some(&public))).await?;
        ensure!(status == StatusCode::OK);
        public_id(&bytes, true)?;
        ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
        Ok(())
    }
    .await;
    first.fixture.service.shutdown().await?;
    second.fixture.service.shutdown().await?;
    provider.close().await?;
    outcome
}

#[tokio::test]
async fn same_installation_tenant_route_and_revision_transplants_do_not_send() -> Result<()> {
    let mut provider = Server::start().await?;
    let mut ready = ready(&provider).await?;
    let (_, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
    let public = public_id(&bytes, false)?;
    provider.wire().await?;
    ready.fixture.service.shutdown().await?;
    let mut alias = ready.route.clone();
    alias.resource_id = "second:literal-route".try_into()?;
    let mut revised = ready.route.clone();
    revised.revision_id = "r2".try_into()?;
    let mut tenant_route = ready.route.clone();
    tenant_route.owner_id = "tenant-b".try_into()?;
    let mut first_slot = ready.route.clone();
    first_slot.resource_kind = ResourceKind::Credential;
    first_slot.resource_id = "upstream-slot".try_into()?;
    let mut second_slot = first_slot.clone();
    second_slot.resource_id = "second-slot".try_into()?;
    let mut tenant_slot = first_slot.clone();
    tenant_slot.owner_id = tenant_route.owner_id.clone();
    let credential = |reference: &ResourceRef| json!({"reference":reference,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}});
    let resources = Resources::with_key(ready.fixture.installation.clone(), [29; 32])?;
    resources.provision_bytes(&ready.fixture.db,&serde_json::to_vec(&json!({"format":1,"installation_id":ready.fixture.installation,"resources":[
        credential(&second_slot),credential(&tenant_slot),route_record(&alias,&first_slot,&provider.url),route_record(&revised,&second_slot,&provider.url),route_record(&tenant_route,&tenant_slot,&provider.url)
    ]}))?).await?;
    let initial = crate::tenancy::store::load(&ready.fixture.db, chrono::Utc::now()).await?;
    ready.fixture.service = Service::start_prepared(
        ready.fixture.db.clone(),
        None,
        Default::default(),
        initial,
        Arc::new(resources),
    )
    .await?;
    ready
        .fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let caller = ready.fixture.issue("transplant-http-caller").await?;
    let secret = caller.secret.context("caller secret")?;
    ready.headers = headers(&ready.fixture, &secret, &ready.revision.revision_id)?;
    let outcome: Result<()> = async {
        let before = provider.state.calls.load(Ordering::SeqCst);
        for (reference, operation) in [
            (&alias, "alias-http-draft"),
            (&revised, "revised-http-draft"),
        ] {
            let revision = draft(&ready.fixture, reference, operation, 32).await?;
            let bound = headers(&ready.fixture, &secret, &revision.revision_id)?;
            let mut input = body(false, Some(&public));
            input["model"] = reference.resource_id.as_str().into();
            let (status, _) = send(&ready, bound, input).await?;
            ensure!(!status.is_success());
        }
        let tenant: bitrouter_ai::tenancy::identity::TenantId = "tenant-b".try_into()?;
        let binding: bitrouter_ai::tenancy::identity::BindingId = "binding-b".try_into()?;
        applied(
            ready
                .fixture
                .service
                .execute(
                    ready.fixture.operator.clone(),
                    Command::Tenant(
                        tenant.clone(),
                        TenantPut {
                            operation_id: "http-tenant-b".try_into()?,
                            expected_revision: None,
                            state: TenantState::Active,
                            installation_id: ready.fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        let binding_result = applied(
            ready
                .fixture
                .service
                .execute(
                    ready.fixture.operator.clone(),
                    Command::Binding(
                        tenant.clone(),
                        binding.clone(),
                        BindingPut {
                            operation_id: "http-binding-b".try_into()?,
                            expected_revision: None,
                            principal_id: ready.fixture.principal.clone(),
                            role: TenantRole::TenantAdmin,
                            scope: crate::tenancy::mutation::tenant_scope(),
                            expires_at: expiry()?,
                            expected_management_owner: None,
                            installation_id: ready.fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        let other = Fixture {
            service: ready.fixture.service.clone(),
            db: ready.fixture.db.clone(),
            operator: ready.fixture.operator.clone(),
            installation: ready.fixture.installation.clone(),
            tenant,
            principal: ready.fixture.principal.clone(),
            binding,
            binding_result,
        };
        let revision = draft(&other, &tenant_route, "tenant-b-http-draft", 32).await?;
        let issued = other.issue("tenant-b-http-caller").await?;
        let bound = headers(
            &other,
            issued.secret.as_deref().context("tenant-b secret")?,
            &revision.revision_id,
        )?;
        let (status, _) = send(&ready, bound, body(false, Some(&public))).await?;
        ensure!(!status.is_success());
        ensure!(provider.state.calls.load(Ordering::SeqCst) == before);
        // All equal names/targets/secrets above remain distinct qualified refs.
        // The original R1 remains usable after those retained resources exist.
        let (status, bytes) =
            send(&ready, ready.headers.clone(), body(true, Some(&public))).await?;
        ensure!(status == StatusCode::OK);
        public_id(&bytes, true)?;
        ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    provider.close().await?;
    outcome
}
