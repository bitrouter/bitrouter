//! Opt-in real backend consumer campaign, not a production PG/MySQL profile.
//! The parent builds the ACP peer separately; this test never invokes Cargo.
use super::*;
use crate::tenancy::capture::port::Port;
use bitrouter_ai::tenancy::identity::{BindingId, RevisionId, TenantId};
use chrono::Utc;
use sea_orm::DatabaseConnection;

mod database;
mod observations;
mod replay;

struct Ready {
    fixture: Fixture,
    route: ResourceRef,
    revision: RevisionId,
    secret: String,
}
impl Ready {
    fn headers(&self) -> Result<HeaderMap> {
        headers(&self.fixture, &self.secret, &self.revision)
    }
}

// Actual resources stay outside the timed/caught campaign. Taking a facade for
// shutdown requests stop on cancellation; its SAME Service still owns cleanup.
struct Consumers {
    directory: tempfile::TempDir,
    program: PathBuf,
    provider: Option<Provider>,
    listeners: Vec<Option<Listener>>,
    sessions: Vec<Option<ControlledSession>>,
    ports: Vec<Arc<Port>>,
    markers: Vec<PathBuf>,
}
impl Consumers {
    fn provider(&self) -> Result<&Provider> {
        self.provider.as_ref().context("provider retained")
    }
    fn session(&self, slot: usize) -> Result<&ControlledSession> {
        self.sessions
            .get(slot)
            .and_then(Option::as_ref)
            .context("controller retained")
    }
    async fn open(&mut self, ready: &Ready) -> Result<usize> {
        let app = Arc::new(
            host_app(
                &ready.fixture,
                &self.provider()?.url,
                Arc::new(HostSpies::default()),
            )?
            .with_tenancy(ready.fixture.service.clone()),
        );
        let listener = Listener::start(app.clone()).await?;
        let origin = listener.origin.clone();
        self.listeners.push(Some(listener));
        let marker = self
            .directory
            .path()
            .join(format!("child-{}.json", self.markers.len()));
        self.markers.push(marker.clone());
        let proposed = launch(&self.program, &marker, &origin, None);
        let session = ControlledSession::open_authenticated(
            app,
            ready.headers()?,
            proposed,
            CancellationToken::new(),
        )
        .await?;
        self.sessions.push(Some(session));
        Ok(self.sessions.len() - 1)
    }
    async fn close_session(&mut self, slot: usize) -> Result<()> {
        let session = self
            .sessions
            .get_mut(slot)
            .and_then(Option::take)
            .context("controller retained for close")?;
        session.shutdown().await?;
        reaped(self.markers.get(slot).context("child marker")?).await
    }
    async fn turn(&mut self, ready: &Ready, slot: usize, native: &str, text: &str) -> Result<()> {
        self.session(slot)?.prompt(native, text).await?;
        tokio::time::timeout(TIMEOUT, ready.fixture.service.admission.wait_drained())
            .await
            .context("real provider execution lease did not drain")?;
        let observed = self
            .provider
            .as_mut()
            .context("provider retained")?
            .receive()
            .await?;
        ensure!(observed.body["model"] == "model-a" && observed.body["max_tokens"] == 25);
        ensure!(
            observed
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                == Some("Bearer owned-upstream-secret")
        );
        Ok(())
    }
    async fn finish(&mut self, database: &database::Fixture) -> Result<()> {
        let mut errors = Vec::new();
        for session in &mut self.sessions {
            drop(session.take());
        }
        for port in &self.ports {
            if let Err(error) = tokio::time::timeout(Duration::from_secs(10), port.finish())
                .await
                .context("capture Port owner did not settle")
                .and_then(|v| v)
            {
                errors.push(format!("{error:#}"));
            }
        }
        // Host-before-actor shutdown is the existing Service implementation.
        // No test task, cancellation flag or timer can attest to a clean owner.
        if let Err(error) = database.quiesce().await {
            errors.push(format!("{error:#}"));
        }
        for marker in &self.markers {
            if marker.exists()
                && let Err(error) = reaped(marker).await
            {
                errors.push(format!("{error:#}"));
            }
        }
        for listener in &mut self.listeners {
            if let Some(listener) = listener.take()
                && let Err(error) = listener.finish().await
            {
                errors.push(format!("{error:#}"));
            }
        }
        if let Some(provider) = self.provider.take()
            && let Err(error) = provider.finish().await
        {
            errors.push(format!("{error:#}"));
        }
        ensure!(
            errors.is_empty(),
            "capture consumers not clean: {}",
            errors.join("; ")
        );
        Ok(())
    }
}

async fn configured(fixture: Fixture, route: ResourceRef, label: &str) -> Result<Ready> {
    let mut input = fixture.configuration_input(&format!("{label}-config"))?;
    input.document.routes.push(route.clone());
    input.document.generation_defaults.max_output_tokens = 25;
    input.document.learning.mode = LearningMode::Observe;
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
        _ => anyhow::bail!("configuration reply"),
    };
    let key = fixture.issue(&format!("{label}-key")).await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    Ok(Ready {
        fixture,
        route,
        revision,
        secret: key.secret.context("new credential secret")?,
    })
}

async fn provision(
    database: &mut database::Fixture,
    db: &DatabaseConnection,
    url: &str,
) -> Result<(Ready, Ready)> {
    let mut a = Fixture::with_database(db.clone(), OperationLimits::default(), &mut |s| {
        database.retain(s)
    })
    .await?;
    let tenant: TenantId = "tenant-b".try_into()?;
    let binding: BindingId = "binding-b".try_into()?;
    result(
        a.service
            .execute(
                a.operator.clone(),
                Command::Tenant(
                    tenant.clone(),
                    TenantPut {
                        operation_id: "capture-create-b".try_into()?,
                        expected_revision: None,
                        state: TenantState::Active,
                        installation_id: a.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let binding_result = result(
        a.service
            .execute(
                a.operator.clone(),
                Command::Binding(
                    tenant.clone(),
                    binding.clone(),
                    BindingPut {
                        operation_id: "capture-bind-b".try_into()?,
                        expected_revision: None,
                        principal_id: a.principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: crate::tenancy::mutation::tenant_scope(),
                        expires_at: expiry()?,
                        expected_management_owner: None,
                        installation_id: a.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let (next, route_a, _) = crate::tenancy::tests::execution::provision_protocol_with_fixture(
        a,
        url,
        "chat_completions",
        &mut |s| database.retain(s),
    )
    .await?;
    a = next;
    let b = Fixture {
        service: a.service.clone(),
        db: db.clone(),
        operator: a.operator.clone(),
        installation: a.installation.clone(),
        tenant,
        principal: a.principal.clone(),
        binding,
        binding_result,
    };
    let (b, route_b, _) = crate::tenancy::tests::execution::provision_protocol_with_fixture(
        b,
        url,
        "chat_completions",
        &mut |s| database.retain(s),
    )
    .await?;
    a.service = b.service.clone();
    Ok((
        configured(a, route_a, "capture-a").await?,
        configured(b, route_b, "capture-b").await?,
    ))
}

async fn facade_pair(consumers: &mut Consumers, a: &Ready, b: &Ready) -> Result<()> {
    let a_slot = consumers.open(a).await?;
    let native_a = consumers
        .session(a_slot)?
        .new_session(consumers.directory.path().to_path_buf())
        .await?;
    consumers
        .turn(a, a_slot, &native_a, "tenant A real backend turn")
        .await?;
    let checkpoint_a = consumers
        .session(a_slot)?
        .checkpoint(&native_a)
        .await?
        .context("tenant A stopped prefix")?;
    ensure!(checkpoint_a.observations >= 5 && checkpoint_a.watermark > 0);
    let foreign_native = consumers
        .session(a_slot)?
        .new_session(consumers.directory.path().to_path_buf())
        .await?;
    ensure!(foreign_native != native_a);
    consumers.close_session(a_slot).await?;
    ensure!(a.fixture.service.host_slots_for_test() == 1);
    let a_before = observations::Graph::read(&a.fixture.db)
        .await?
        .tenant(a.fixture.tenant.as_str());

    let b_slot = consumers.open(b).await?;
    let native_b = consumers
        .session(b_slot)?
        .new_session(consumers.directory.path().to_path_buf())
        .await?;
    ensure!(
        native_a == native_b,
        "fixture must actually reuse the visible native name"
    );
    consumers
        .turn(b, b_slot, &native_b, "tenant B same native name")
        .await?;
    let checkpoint_b = consumers
        .session(b_slot)?
        .checkpoint(&native_b)
        .await?
        .context("tenant B stopped prefix")?;
    ensure!(
        checkpoint_a.checkpoint_id != checkpoint_b.checkpoint_id
            && checkpoint_a.prefix_digest != checkpoint_b.prefix_digest
    );
    ensure!(consumers.session(b_slot)?.checkpoint(&native_b).await? == Some(checkpoint_b.clone()));
    let before = observations::Graph::read(&b.fixture.db).await?;
    let a_session = before.session(a.fixture.tenant.as_str(), &native_a)?;
    let b_session = before.session(b.fixture.tenant.as_str(), &native_b)?;
    ensure!(
        a_session.session_key != b_session.session_key
            && a_session.owner == b_session.owner
            && a_session.source == b_session.source
    );
    ensure!(
        a_session.installation_id.as_deref() == Some(a.fixture.installation.as_str())
            && b_session.installation_id == a_session.installation_id
    );
    ensure!(
        before.tenant(a.fixture.tenant.as_str()) == a_before,
        "tenant B changed tenant A graph"
    );
    let calls = consumers.provider()?.calls();
    ensure!(
        consumers
            .session(b_slot)?
            .prompt(&foreign_native, "must not cross controller namespace")
            .await
            .is_err()
    );
    consumers.close_session(b_slot).await?;
    ensure!(consumers.provider()?.calls() == calls && calls == 2);
    let after = observations::Graph::read(&b.fixture.db).await?;
    after.closed()?;
    ensure!(after.tenant(a.fixture.tenant.as_str()) == a_before);
    ensure!(
        after.checkpoints == before.checkpoints
            && after.members == before.members
            && after.records == before.records
    );
    ensure!(b.fixture.service.current().is_ok() && b.fixture.service.host_slots_for_test() == 1);
    ensure!(
        crate::acp_trajectory::CanonicalStore::new(a.fixture.db.clone())
            .list(a.fixture.principal.as_str(), "capture-fixture")
            .await?
            .is_empty()
    );
    Ok(())
}

async fn campaign(database: &mut database::Fixture, consumers: &mut Consumers) -> Result<()> {
    let db = database.connect().await?;
    let (mut a, b) = provision(database, &db, &consumers.provider()?.url).await?;
    facade_pair(consumers, &a, &b).await?;
    replay::exact_then_conflicting(consumers, &a).await?;
    let committed = observations::Graph::read(&db).await?;
    committed.closed()?;
    database.close_pools().await?;
    let fresh = database.connect().await?;
    ensure!(
        observations::Graph::read(&fresh).await? == committed,
        "committed graph changed across physical pool closure/reconnect"
    );
    let initial = store::load(&fresh, Utc::now()).await?;
    a.fixture.service = Service::start_prepared(
        fresh.clone(),
        None,
        OperationLimits::default(),
        initial,
        Arc::new(Resources::with_key(
            a.fixture.installation.clone(),
            [29; 32],
        )?),
    )
    .await?;
    database.retain(&a.fixture.service);
    a.fixture.db = fresh.clone();
    a.fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    // Fresh host authentication uses persisted authority/config; no old proof,
    // live flag reset, historical-session import or resumption authority is reused.
    let slot = consumers.open(&a).await?;
    let native = consumers
        .session(slot)?
        .new_session(consumers.directory.path().to_path_buf())
        .await?;
    ensure!(native == "native-1");
    consumers
        .turn(&a, slot, &native, "new controller after SQL reconnect")
        .await?;
    ensure!(
        consumers
            .session(slot)?
            .checkpoint(&native)
            .await?
            .is_some()
    );
    consumers.close_session(slot).await?;
    ensure!(consumers.provider()?.calls() == 3 && a.fixture.service.host_slots_for_test() == 1);
    replay::foreign_namespace(consumers, &a, b.fixture.tenant.as_str()).await?;
    observations::Graph::read(&fresh).await?.closed()?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL (PG/MySQL CREATE/DROP) and separately built BITROUTER_OWNED_CAPTURE_FIXTURE"]
async fn server_capture03_actor_facade_namespace_replay_and_reconnect() -> Result<()> {
    let program = executable()?;
    let directory = tempfile::tempdir()?;
    let mut database = database::Fixture::create().await?;
    let provider = match Provider::start(false).await {
        Ok(value) => value,
        Err(error) => return database.finish(Err(error), true).await,
    };
    let mut consumers = Consumers {
        directory,
        program,
        provider: Some(provider),
        listeners: Vec::new(),
        sessions: Vec::new(),
        ports: Vec::new(),
        markers: Vec::new(),
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(120),
        std::panic::AssertUnwindSafe(campaign(&mut database, &mut consumers)).catch_unwind(),
    )
    .await
    .context("capture backend campaign deadline")
    .and_then(|value| value.map_err(|_| anyhow::anyhow!("capture backend campaign unwound")))
    .and_then(|value| value);
    let cleanup = consumers.finish(&database).await;
    let consumers_clean = cleanup.is_ok();
    let outcome = match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("consumer cleanup: {cleanup:#}"))),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    };
    database.finish(outcome, consumers_clean).await
}
