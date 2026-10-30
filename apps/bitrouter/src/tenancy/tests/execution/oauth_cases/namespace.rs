use super::*;
use sea_orm::{ConnectionTrait, TransactionTrait};

#[tokio::test]
async fn equal_seed_in_distinct_tenant_slots_is_importable_and_rotation_is_physically_isolated()
-> Result<()> {
    let harness = Harness::new(true).await?;
    harness.fixture.service.shutdown().await?;
    let mut imported: Value = serde_json::from_slice(&harness.manifest)?;
    let mut foreign = harness.slot.clone();
    foreign.owner_id = "other-tenant".try_into()?;
    imported["resources"] = json!([{ "reference":foreign,"definition":imported["resources"][0]["definition"].clone() }]);
    harness
        .resources
        .provision_bytes(&harness.fixture.db, &serde_json::to_vec(&imported)?)
        .await?;
    let first = harness.row().await?;
    let second = storage::load(&harness.fixture.db, &harness.fixture.installation, &foreign)
        .await?
        .context("independent equal seed")?;
    let crypto = harness.resources.oauth_crypto()?;
    ensure!(
        first.token(&crypto)? == second.token(&crypto)?
            && first.metadata.slot != second.metadata.slot
    );
    let mut rotated = first.metadata.clone();
    rotated.generation = 1;
    let token = crate::gateway::providers::auth::credentials::OAuthToken {
        access_token: "isolated-rotation".into(),
        refresh_token: Some("isolated-refresh".into()),
        expires_at: u64::try_from(chrono::Utc::now().timestamp())? + 3600,
    };
    let replacement = storage::Row::new(&crypto, rotated, &token)?;
    let transaction = harness.fixture.db.begin().await?;
    storage::replace(&transaction, &first, &replacement).await?;
    transaction.commit().await?;
    ensure!(harness.row().await?.token(&crypto)?.access_token == "isolated-rotation");
    let preserved = storage::load(&harness.fixture.db, &harness.fixture.installation, &foreign)
        .await?
        .context("second")?;
    ensure!(
        preserved.metadata.generation == 0
            && preserved.token(&crypto)?.access_token == "seed-access"
    );
    ensure!(
        storage::load(
            &harness.fixture.db,
            &"foreign-installation".try_into()?,
            &foreign
        )
        .await?
        .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn missing_slot_never_reconstructs_seed_and_new_authority_revision_cannot_rebind_it()
-> Result<()> {
    let harness = Harness::new(true).await?;
    harness.fixture.service.shutdown().await?;
    let mut other: Value = serde_json::from_slice(&harness.manifest)?;
    other["resources"][0]["reference"]["revision_id"] = json!("r2");
    other["resources"]
        .as_array_mut()
        .context("resources")?
        .truncate(1);
    ensure!(
        harness
            .resources
            .provision_bytes(&harness.fixture.db, &serde_json::to_vec(&other)?)
            .await
            .is_err()
    );
    let original = harness.row().await?;
    ensure!(original.metadata.generation == 0);
    harness
        .fixture
        .db
        .execute_unprepared("DELETE FROM tenancy_oauth_slots")
        .await?;
    let result = Service::start_prepared(
        harness.fixture.db.clone(),
        None,
        crate::tenancy::actor::OperationLimits::default(),
        crate::tenancy::store::load(&harness.fixture.db, chrono::Utc::now()).await?,
        harness.resources.clone(),
    )
    .await;
    ensure!(result.is_err());
    // A direct identical reimport may not repair a missing live slot either.
    ensure!(
        harness
            .resources
            .provision_bytes(&harness.fixture.db, &harness.manifest)
            .await
            .is_err()
    );
    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) == 0);
    Ok(())
}

#[tokio::test]
async fn swapped_ciphertext_and_generation_cannot_authorize_refresh_or_model() -> Result<()> {
    let harness = Harness::new(true).await?;
    harness
        .fixture
        .db
        .execute_unprepared("UPDATE tenancy_oauth_slots SET generation=1")
        .await?;
    let response = harness.send(&Harness::client(false)?, false).send().await?;
    ensure!(response.status() != StatusCode::OK);
    response.bytes().await?;
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 0
            && harness.provider.models.load(Ordering::SeqCst) == 0
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn unsupported_responses_oauth_remains_fenced_before_any_refresh() -> Result<()> {
    let harness = Harness::new(true).await?;
    let submission = harness
        .fixture
        .service
        .submission(&harness.headers)
        .map_err(failure)?;
    let execution = harness
        .fixture
        .service
        .admit_execution(
            submission,
            MODEL,
            &bitrouter_ai::types::ApiProtocol::Responses,
        )
        .await;
    if let Ok(execution) = execution {
        use crate::gateway::pipeline::routing::{RoutingPrefs, RoutingTable};
        let target = execution
            .route()
            .route_chain(MODEL, &RoutingPrefs::default(), &execution.caller())
            .await?
            .into_iter()
            .next()
            .context("candidate")?;
        ensure!(matches!(
            execution.oauth_profile(&target),
            Err(bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch)
        ));
    }
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 0
            && harness.provider.models.load(Ordering::SeqCst) == 0
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

async fn tenant_with_pool(
    harness: &Harness,
    name: &str,
    slot: &ResourceRef,
    granted: bool,
) -> Result<Fixture> {
    use crate::tenancy::{
        command::Command,
        tests::{expiry, result},
    };
    use bitrouter_ai::tenancy::authority::{
        BindingPut, GrantScope, TenantPut, TenantRole, TenantState,
    };
    let first = &harness.fixture;
    let tenant: bitrouter_ai::tenancy::identity::TenantId = name.try_into()?;
    result(
        first
            .service
            .execute(
                first.operator.clone(),
                Command::Tenant(
                    tenant.clone(),
                    TenantPut {
                        operation_id: format!("create-{name}").try_into()?,
                        expected_revision: None,
                        state: TenantState::Active,
                        installation_id: first.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let scope = GrantScope::Tenant {
        operator_pools: if granted { vec![slot.key()] } else { vec![] },
    };
    let binding_result = result(
        first
            .service
            .execute(
                first.operator.clone(),
                Command::Binding(
                    tenant.clone(),
                    first.binding.clone(),
                    BindingPut {
                        operation_id: format!("bind-{name}").try_into()?,
                        expected_revision: None,
                        principal_id: first.principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope,
                        expires_at: expiry()?,
                        expected_management_owner: None,
                        installation_id: first.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
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

#[tokio::test]
async fn explicit_pool_shares_one_refresh_generation_but_equal_tenant_seed_and_ungranted_tenant_do_not()
-> Result<()> {
    use crate::tenancy::{command::Command, tests::issued};
    use bitrouter_ai::tenancy::authority::GrantScope;
    let mut harness = Harness::new(true).await?;
    harness.fixture.service.shutdown().await?;
    let mut pool = harness.slot.clone();
    pool.owner_kind = OwnerKind::OperatorPool; // Same owner text/name/seed; kind separates it.
    let original: Value = serde_json::from_slice(&harness.manifest)?;
    let mut imports =
        vec![json!({"reference":pool,"definition":original["resources"][0]["definition"]})];
    let names = ["tenant-pool-b", "tenant-pool-c", "tenant-no-pool"];
    let mut routes = Vec::new();
    for name in names {
        let mut route = harness.route.clone();
        route.owner_id = name.try_into()?;
        let mut definition = original["resources"][1]["definition"].clone();
        definition["credentials"][0]["credential"] = serde_json::to_value(&pool)?;
        imports.push(json!({"reference":route,"definition":definition}));
        routes.push(route);
    }
    harness
        .resources
        .provision_bytes(
            &harness.fixture.db,
            &serde_json::to_vec(&json!({
                "format":1,"installation_id":harness.fixture.installation,"resources":imports,
            }))?,
        )
        .await?;
    harness.reopen().await?;
    harness.provider.refresh_gate.add_permits(1);
    harness.provider.model_gate.add_permits(2);
    for (index, (name, route)) in names.into_iter().zip(&routes).enumerate() {
        let granted = index < 2;
        let tenant = tenant_with_pool(&harness, name, &pool, granted).await?;
        let revision = draft(&tenant, route, &format!("draft-{name}"), 25).await?;
        let mut input = tenant.issuance(&format!("issue-{name}"))?;
        if granted {
            input.scope = GrantScope::Tenant {
                operator_pools: vec![pool.key()],
            };
        }
        let key = issued(
            tenant
                .service
                .execute(
                    tenant.operator.clone(),
                    Command::Issue(tenant.tenant.clone(), input),
                )
                .await
                .map_err(failure)?,
        )?;
        let binding = headers(
            &tenant,
            key.secret.as_deref().context("pool secret")?,
            &revision.revision_id,
        )?;
        let response = Harness::client(cfg!(feature = "http2") && index == 1)?
            .post(format!("{}/v1/messages",harness.base)).headers(binding)
            .json(&json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25}))
            .send().await?;
        let status = response.status();
        let id = if granted {
            Some(execution_id(&response)?)
        } else {
            None
        };
        response.bytes().await?;
        if granted {
            let id = id.context("granted execution")?;
            ensure!(status == StatusCode::OK, "pool response {status}");
            let receipt = crate::tenancy::receipts::store::lookup(
                &tenant.db,
                &tenant.installation,
                &tenant.tenant,
                &id,
            )
            .await?
            .context("pool receipt")?;
            ensure!(
                receipt.tenant_id == tenant.tenant
                    && receipt.principal_id == tenant.principal
                    && receipt.attempts.len() == 1
            );
        } else {
            ensure!(
                status == StatusCode::FORBIDDEN,
                "ungranted pool response {status}"
            );
        }
    }
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 1
            && harness.provider.models.load(Ordering::SeqCst) == 2
    );
    let shared = storage::load(&harness.fixture.db, &harness.fixture.installation, &pool)
        .await?
        .context("pool slot")?;
    ensure!(shared.metadata.generation == 1);
    let separate = harness.row().await?;
    ensure!(
        separate.metadata.generation == 0
            && separate
                .token(&harness.resources.oauth_crypto()?)?
                .access_token
                == "seed-access"
    );
    harness.fixture.service.shutdown().await?;
    harness.close().await
}

#[tokio::test]
async fn changed_seed_or_account_within_same_slot_cannot_reset_ready_lineage() -> Result<()> {
    let harness = Harness::new(false).await?;
    harness.fixture.service.shutdown().await?;
    let before = harness.row().await?;
    for account in [false, true] {
        let mut manifest: Value = serde_json::from_slice(&harness.manifest)?;
        manifest["resources"]
            .as_array_mut()
            .context("resources")?
            .truncate(1);
        if account {
            manifest["resources"][0]["definition"]["oauth"]["account"] = json!("other-account");
        } else {
            manifest["resources"][0]["definition"]["credential"]["data"]["refresh_token"] =
                json!("different-seed");
        }
        ensure!(
            harness
                .resources
                .provision_bytes(&harness.fixture.db, &serde_json::to_vec(&manifest)?)
                .await
                .is_err()
        );
        ensure!(before.same(&harness.row().await?));
    }
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 0
            && harness.provider.models.load(Ordering::SeqCst) == 0
    );
    harness.close().await
}
