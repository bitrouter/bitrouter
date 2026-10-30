use super::*;

#[tokio::test]
#[ignore = "requires an empty disposable BITROUTER_TEST_NATIVE_DATABASE_URL (PG/MySQL), through 036"]
async fn server_owned_choice_actual_consumer_restores_exact_profile_and_receipt_links() -> Result<()>
{
    let db = super::super::server::connect().await?;
    let directory = tempfile::tempdir()?;
    let mut generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(true, false).await?;
    let mut services = Vec::new();
    let checked=tokio::time::timeout(Duration::from_secs(90),std::panic::AssertUnwindSafe(async {
        let mut bound=fixture::prepare(db.clone(),None,&generation,&choice,2,&mut |service|services.push(service.clone())).await
            .context("Choice backend: provision exact profile")?;
        positive(&bound,&mut generation,&mut choice,directory.path(),false).await
            .context("Choice backend: actual Decisions and Generation")?;
        let before=fixture::journal(&bound).await?;
        let links=before["owned_choices"].clone();
        let next=draft(&bound.fixture,&bound.route,"choice-r2",7).await?;
        result(bound.fixture.service.execute(bound.fixture.operator.clone(),Command::Activate(bound.fixture.tenant.clone(),
            bitrouter_ai::tenancy::configuration::Activation{operation_id:"choice-activate-r2".try_into()?,expected_active_revision:None,
                revision_id:next.revision_id,expected_authority_revision:bound.fixture.service.current().map_err(failure)?.revision,
                installation_id:bound.fixture.installation.clone()})).await.map_err(failure)?)?;
        let rotated=bound.fixture.issue("choice-rotated").await?;
        bound.headers=crate::tenancy::tests::execution::headers(&bound.fixture,
            rotated.secret.as_deref().context("rotated key")?,&bound.revision)?;
        let restored=fixture::open(&bound,&generation,directory.path(),Arc::new(HostSpies::default())).await
            .context("Choice backend: authenticated full journal restore")?;
        ensure!(restored.core().snapshot().await.context_store.decisions.len()==1);
        ensure!(fixture::journal(&bound).await?["owned_choices"]==links);
        ensure!(bound.fixture.service.admission.ingress_for_test().map_err(|code|anyhow::anyhow!("{code:?}"))?.2==0,
            "idle restored session acquired an execution lease");
        restored.close().await?;
        let after=fixture::journal(&bound).await?;
        ensure!(after["owned_choices"]==links && after["released"]==true);
        ensure!(choice.calls()==1 && generation.calls()==1,"restoration replayed paid work");
        let row=db.query_one(Statement::from_string(db.get_database_backend(),
            "SELECT execution_revision, installation_id, tenant_id, session_principal_id FROM bro_managed_sessions"))
            .await?.context("retained owned row")?;
        ensure!(row.try_get::<String>("","execution_revision")?==bound.revision.as_str());
        ensure!(row.try_get::<String>("","installation_id")?==bound.fixture.installation.as_str()
            && row.try_get::<String>("","tenant_id")?==bound.fixture.tenant.as_str()
            && row.try_get::<String>("","session_principal_id")?==bound.fixture.principal.as_str());
        Ok::<_,anyhow::Error>(())
    }).catch_unwind()).await.context("Choice backend deadline")
        .and_then(|value|value.map_err(|_|anyhow::anyhow!("Choice backend unwound"))).and_then(|value|value);
    // Keep all actual actors, pool and socket tasks outside failure/timeout scope.
    // No destructive DB cleanup or successful owner ACK is inferred from joining.
    let mut cleanup = Vec::new();
    choice.release();
    for service in &services {
        if let Err(error) = tokio::time::timeout(Duration::from_secs(35), service.shutdown())
            .await
            .context("Choice backend owner drain deadline")
            .and_then(|result| result)
        {
            cleanup.push(error.to_string());
        }
    }
    if let Err(error) = choice.finish().await {
        cleanup.push(error.to_string());
    }
    if let Err(error) = generation.finish().await {
        cleanup.push(error.to_string());
    }
    if cleanup.is_empty()
        && let Err(error) = tokio::time::timeout(TIMEOUT, db.close())
            .await
            .context("Choice backend pool close deadline")
            .and_then(|result| result.map_err(anyhow::Error::from))
    {
        cleanup.push(error.to_string());
    }
    ensure!(
        cleanup.is_empty(),
        "Choice backend custody unconfirmed; retain database: {}; test: {checked:?}",
        cleanup.join("; ")
    );
    checked
}
