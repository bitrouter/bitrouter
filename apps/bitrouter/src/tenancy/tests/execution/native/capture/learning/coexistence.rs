//! Both checked consumers use one retained parent without sharing child authority.
use super::*;
use crate::gateway::pipeline::routing::{RoutingPrefs, RoutingTable};
use crate::tenancy::tests::execution::native::session::choice::{self, fixture::Bound};
use bitrouter_ai::{
    tenancy::authority::{OwnerKind, ResourceKind},
    types::ModelOperation,
};

const JUDGE: &str = "separate-assessment-judge";

async fn combined(
    provider: &Provider,
    choice: &choice::wire::Endpoint,
    judge: &Judge,
    root: &Path,
) -> Result<Bound> {
    let url = format!("sqlite://{}", root.join("owned.db").display());
    let lease = crate::tenancy::ownership::acquire(&url)?;
    let db = crate::db::connect_tenancy(&url, &lease).await?;
    let mut fixture = Fixture::with_database(db, OperationLimits::default(), &mut |_| {}).await?;
    fixture.service.shutdown().await?;
    let resources = Resources::with_key(fixture.installation.clone(), [29; 32])?;
    let reference = |kind, id: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "joint-r1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, MODEL)?;
    let choice_route = reference(ResourceKind::Route, choice::fixture::CHILD)?;
    let judge_route = reference(ResourceKind::Route, JUDGE)?;
    let generation_key = reference(ResourceKind::Credential, "parent-key")?;
    let choice_key = reference(ResourceKind::Credential, "choice-key")?;
    let judge_key = reference(ResourceKind::Credential, "assessment-key")?;
    let route_row = |reference: &ResourceRef,
                     key: &ResourceRef,
                     url: &str,
                     model: &str,
                     protocol: &str| {
        json!({"reference":reference,"definition":{
            "kind":"route","selector":format!("mock:{model}"),"policy":null,
            "routing":{"providers":{"mock":{"api_base":url,"api_protocol":[{"*":protocol}],"models":[{"id":model}]}}},
            "credentials":[{"provider":"mock","account":"default","credential":key}]
        }})
    };
    let secret = |reference: &ResourceRef, value: &str| {
        json!({"reference":reference,"definition":{"kind":"credential",
            "credential":{"type":"api_key","data":{"value":value}}}})
    };
    let mut parent = route_row(
        &route,
        &generation_key,
        &provider.url,
        "model-a",
        "chat_completions",
    );
    parent["definition"]["core_choice"] = json!({
        "profile":"choice_v1","route":choice_route,"policy":{}
    });
    parent["definition"]["assessment"] = json!({
        "profile":"assessment_v1","route":judge_route
    });
    // One real operator import, parent first. No second provisioner rewrites or
    // silently drops either optional field after the other was authenticated.
    resources
        .provision_bytes(
            &fixture.db,
            &serde_json::to_vec(&json!({
                "format":1,"installation_id":fixture.installation,"resources":[parent,
                    route_row(&choice_route,&choice_key,&choice.url,"judge-r1","decisions"),
                    route_row(&judge_route,&judge_key,&judge.url,"judge-model","chat_completions"),
                    secret(&generation_key,"owned-upstream-secret"),
                    secret(&choice_key,"owned-choice-secret"),
                    secret(&judge_key,"explicit-judge-secret")]
            }))?,
        )
        .await?;
    let snapshot = store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        Some(lease),
        OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    let mut input = fixture.configuration_input("combined-profile-r1")?;
    input.document.routes = vec![route.clone(), judge_route];
    input.document.limits.active_executions = 2;
    input.document.limits.queued_work = 0;
    input.document.limits.learning_jobs = 2;
    input.document.learning.mode = LearningMode::Observe;
    input.document.learning.max_evaluations = 1;
    input.document.generation_defaults.max_output_tokens = 25;
    let revision = match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        crate::tenancy::store::StoredResponse::Configuration(value) => value.revision_id,
        _ => anyhow::bail!("combined configuration response"),
    };
    let key = fixture.issue("combined-profile-key").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(&fixture, key.secret.as_deref().context("key")?, &revision)?;
    Ok(Bound {
        fixture,
        route,
        headers,
        key,
        revision,
    })
}

async fn separate_proofs(bound: &Bound) -> Result<()> {
    let authority = bound
        .fixture
        .service
        .host_authority(&bound.headers)
        .map_err(failure)?;
    let retained = authority.retained().await.map_err(failure)?;
    let checked = |error| anyhow::anyhow!("retained child proof: {error:?}");
    let parent = retained
        .select_native(MODEL, ModelOperation::Generation)
        .map_err(checked)?;
    let choice = parent.choice_route().map_err(checked)?;
    let judge = retained
        .select_native(JUDGE, ModelOperation::Generation)
        .map_err(checked)?;
    ensure!(parent.assessment_reference() == Some(judge.reference()));
    ensure!(
        parent
            .choice_profile()
            .is_some_and(|profile| &profile.child == choice.reference())
    );
    ensure!(
        retained
            .select_native(choice::fixture::CHILD, ModelOperation::Decisions)
            .is_err(),
        "hidden Choice dependency became an exposed root"
    );
    let caller = CallerContext::local();
    let mut targets = Vec::new();
    for (route, operation, model) in [
        (&parent, ModelOperation::Generation, "model-a"),
        (&choice, ModelOperation::Decisions, "judge-r1"),
        (&judge, ModelOperation::Generation, "judge-model"),
    ] {
        let chain = route
            .route_chain(
                route.reference().resource_id.as_str(),
                &RoutingPrefs {
                    operation,
                    inbound_protocol: None,
                    ..Default::default()
                },
                &caller,
            )
            .await?;
        ensure!(chain.len() == 1 && chain[0].service_id == model);
        targets.push(chain[0].clone());
    }
    let routes = [&parent, &choice, &judge];
    let mut credentials = std::collections::BTreeSet::new();
    for (i, route) in routes.iter().enumerate() {
        credentials.insert(serde_json::to_string(
            &route.match_candidate(&targets[i]).map_err(checked)?,
        )?);
        for (j, target) in targets.iter().enumerate() {
            if i != j {
                ensure!(
                    route.match_candidate(target).is_err(),
                    "sibling child authority was unioned"
                );
            }
        }
    }
    ensure!(credentials.len() == 3);
    Ok(())
}

async fn restart(owner: &mut Option<Bound>, root: &Path) -> Result<()> {
    let bound = owner.as_ref().context("combined owner before restart")?;
    tokio::time::timeout(Duration::from_secs(35), bound.fixture.service.shutdown()).await??;
    let url = format!("sqlite://{}", root.join("owned.db").display());
    ensure!(
        crate::tenancy::ownership::acquire(&url).is_err(),
        "the original pool must retain ownership after service drain"
    );
    tokio::time::timeout(Duration::from_secs(35), bound.fixture.db.clone().close()).await??;
    let Bound {
        fixture,
        route,
        headers: previous_headers,
        key,
        revision,
    } = owner.take().context("closed combined owner")?;
    let Fixture {
        service,
        db,
        operator,
        installation,
        tenant,
        principal,
        binding,
        binding_result,
    } = fixture;
    // Pool clones retain the file lease even after close. Drop the old Service
    // and its last test-side pool handle before acquiring a fresh owner.
    drop((service, db));
    let lease = crate::tenancy::ownership::acquire(&url)?;
    let db = crate::db::connect_tenancy(&url, &lease).await?;
    let resources = Resources::with_key(installation.clone(), [29; 32])?;
    let snapshot = store::load(&db, Utc::now()).await?;
    let service = Service::start_prepared(
        db.clone(),
        Some(lease),
        OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    service.dispatch_test_ready.store(true, Ordering::Release);
    *owner = Some(Bound {
        fixture: Fixture {
            service,
            db,
            operator,
            installation,
            tenant,
            principal,
            binding,
            binding_result,
        },
        route,
        headers: previous_headers,
        key,
        revision,
    });
    let bound = owner.as_mut().context("reopened combined owner")?;
    ensure!(
        matches!(bound.fixture.service.host_authority(&bound.headers),
            Err(error) if error.0.code == ErrorCode::CapabilityMismatch),
        "a fresh Service must reject the previous instance binding"
    );
    bound.headers = headers(
        &bound.fixture,
        bound
            .key
            .secret
            .as_deref()
            .context("retained combined key")?,
        &bound.revision,
    )?;
    separate_proofs(bound).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE and both owned profile consumers"]
async fn joint_choice_and_assessment_use_one_imported_revision_with_separate_child_receipts()
-> Result<()> {
    let program = executable()?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("native-workspace");
    std::fs::create_dir(&workspace)?;
    let mut provider = Provider::start(false).await?;
    let mut choice_wire = choice::wire::Endpoint::start(true, false).await?;
    let mut judge = Judge::new(false).await?;
    let mut owner = Some(combined(&provider, &choice_wire, &judge, directory.path()).await?);
    let mut controller = None;
    let mut listener = None;
    let mut markers = Vec::new();
    let checked = tokio::time::timeout(Duration::from_secs(90), async {
        let mut bound = owner.as_mut().context("combined owner")?;
        separate_proofs(bound).await?;
        choice::positive(bound, &mut provider, &mut choice_wire, &workspace, false).await?;
        ensure!(
            bodies(&bound.fixture, "owned_judge_budget")
                .await?
                .is_empty(),
            "Choice consumed the learning budget"
        );
        let before = bound
            .fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT payload FROM bro_managed_sessions",
            ))
            .await?
            .context("Choice journal")?;
        let before: Value = serde_json::from_str(&before.try_get::<String>("", "payload")?)?;
        let links = before["journal"]["owned_choices"].clone();
        restart(&mut owner, directory.path()).await?;
        bound = owner.as_mut().context("owner after Choice restart")?;
        // The new Service/Resources must reopen the authenticated retained bytes;
        // no process-local route cache or earlier authority survives this step.
        let restored =
            choice::fixture::open(bound, &provider, &workspace, Arc::new(HostSpies::default()))
                .await?;
        ensure!(
            restored
                .core()
                .snapshot()
                .await
                .context_store
                .decisions
                .len()
                == 1
        );
        restored.close().await?;
        ensure!(choice_wire.calls() == 1 && provider.calls() == 1);
        for attempt in 0..2 {
            let spies = Arc::new(HostSpies::default());
            let app = Arc::new(
                host_app(&bound.fixture, &provider.url, spies.clone())?
                    .with_tenancy(bound.fixture.service.clone()),
            );
            listener = Some(Listener::start(app.clone()).await?);
            let marker = directory.path().join(format!("joint-child-{attempt}.json"));
            controller = Some(
                ControlledSession::open_authenticated(
                    app,
                    bound.headers.clone(),
                    launch(
                        &program,
                        &marker,
                        &listener.as_ref().context("listener")?.origin,
                        None,
                    ),
                    CancellationToken::new(),
                )
                .await?,
            );
            markers.push(marker);
            let current = controller.as_ref().context("controller")?;
            let native = current.new_session(directory.path().to_owned()).await?;
            current
                .prompt(&native, &format!("Explicit assessment {attempt}"))
                .await?;
            let sent = provider.receive().await?;
            ensure!(sent.body["model"] == "model-a" && sent.body["max_tokens"] == 25);
            ensure!(
                sent.headers
                    .get("authorization")
                    .is_some_and(|value| value == "Bearer owned-upstream-secret")
            );
            tokio::time::timeout(TIMEOUT, bound.fixture.service.admission.wait_drained()).await?;
            ensure!(judge.calls.load(Ordering::SeqCst) == attempt);
            let assessment = current.checkpoint(&native).await;
            if attempt == 0 {
                ensure!(assessment?.is_some());
                let sent = judge.receive().await?;
                ensure!(sent.body["model"] == "judge-model" && sent.path == "/v1/chat/completions");
                ensure!(
                    sent.headers
                        .get("authorization")
                        .is_some_and(|value| value == "Bearer explicit-judge-secret")
                );
            } else {
                ensure!(
                    assessment.is_err(),
                    "restart refunded the accepted assessment unit"
                );
            }
            tokio::time::timeout(TIMEOUT, bound.fixture.service.admission.wait_drained()).await?;
            ensure!(judge.calls.load(Ordering::SeqCst) == 1 && choice_wire.calls() == 1);
            let budgets = bodies(&bound.fixture, "owned_judge_budget").await?;
            ensure!(budgets.len() == 1 && budgets[0]["accepted"] == 1);
            let jobs = bodies(&bound.fixture, "owned_judge_job").await?;
            ensure!(jobs.len() == 1 && jobs[0]["status"] == "completed");
            let execution = jobs[0]["execution"]
                .as_str()
                .context("assessment execution")?;
            let linked = links
                .as_object()
                .context("Choice links")?
                .values()
                .next()
                .context("Choice link")?;
            ensure!(
                linked["child_execution"] != execution && linked["parent_execution"] != execution
            );
            let row = bound
                .fixture
                .db
                .query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT receipt_json FROM tenancy_execution_receipts WHERE execution_id=?",
                    [execution.into()],
                ))
                .await?
                .context("assessment receipt")?;
            let receipt: Value = serde_json::from_str(&row.try_get::<String>("", "receipt_json")?)?;
            ensure!(
                receipt["execution_revision"] == bound.revision.as_str()
                    && receipt["attempts"]
                        .as_array()
                        .is_some_and(|attempts| attempts.len() == 1)
            );
            ensure!(
                receipt["usage"]["input_tokens"] == 4 && receipt["usage"]["output_tokens"] == 2
            );
            ensure!(
                spies.decisions.load(Ordering::SeqCst) == 0
                    && spies.transforms.load(Ordering::SeqCst) == 0
            );
            controller
                .take()
                .context("controller owner")?
                .shutdown()
                .await?;
            listener.take().context("listener owner")?.finish().await?;
            reaped(markers.last().context("marker")?).await?;
            ensure!(bound.fixture.service.host_slots_for_test() == 1);
            if attempt == 0 {
                restart(&mut owner, directory.path()).await?;
                bound = owner.as_mut().context("owner after assessment restart")?;
            }
        }
        ensure!(provider.calls() == 3 && choice_wire.calls() == 1);
        let after = bound
            .fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT payload FROM bro_managed_sessions",
            ))
            .await?
            .context("same native journal")?;
        let after: Value = serde_json::from_str(&after.try_get::<String>("", "payload")?)?;
        ensure!(after["journal"]["owned_choices"] == links && after["journal"]["released"] == true);
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("joint consumer deadline")
    .and_then(|result| result);
    // Every actual child owner stays outside the timed body; no detached work or
    // successful cleanup is inferred from timeout. Parent retains DB on failure.
    let closed = match controller {
        Some(owner) => tokio::time::timeout(Duration::from_secs(35), owner.shutdown())
            .await
            .context("joint child cleanup deadline")
            .and_then(|value| value),
        None => Ok(()),
    };
    let stopped = match listener {
        Some(owner) => owner.finish().await,
        None => Ok(()),
    };
    let mut reaped_all = Ok(());
    for marker in &markers {
        reaped_all = reaped_all.and(reaped(marker).await);
    }
    let judge_closed = judge.stop().await;
    let choice_closed = choice_wire.finish().await;
    let shutdown = match owner.as_ref() {
        Some(bound) => {
            let stopped =
                tokio::time::timeout(Duration::from_secs(35), bound.fixture.service.shutdown())
                    .await
                    .context("joint service drain deadline")
                    .and_then(|value| value);
            let closed =
                tokio::time::timeout(Duration::from_secs(35), bound.fixture.db.clone().close())
                    .await
                    .context("joint database close deadline")
                    .and_then(|value| value.map_err(Into::into));
            stopped.and(closed)
        }
        None => Err(anyhow::anyhow!(
            "joint restart incomplete; retain original database"
        )),
    };
    let provider_closed = provider.finish().await;
    let cleanup = closed
        .and(stopped)
        .and(reaped_all)
        .and(judge_closed)
        .and(choice_closed)
        .and(shutdown)
        .and(provider_closed);
    let result = checked.and(cleanup);
    if result.is_err() {
        // A failed body may have cancelled the original physical close. A later
        // empty-pool close cannot prove that lost acknowledgement, so retain it.
        let retained = directory.keep();
        return result.with_context(|| {
            format!(
                "joint consumer or cleanup failed; retain fixtures at {}",
                retained.display()
            )
        });
    }
    result
}
