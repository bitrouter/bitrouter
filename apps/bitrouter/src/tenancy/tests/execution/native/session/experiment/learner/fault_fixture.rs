use super::*;
use crate::tenancy::{error::Failure, transaction::Disposition};
use futures::future::BoxFuture;
use sea_orm::sea_query::{Alias, Expr, Query};

pub(super) struct Case<'a> {
    pub fixture: &'a Fixture,
    pub credential: &'a CredentialIssued,
    pub input: RegistrationInput,
}

pub(super) async fn with_case(
    action: impl for<'a> FnOnce(Case<'a>) -> BoxFuture<'a, Result<()>>,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, credential) =
            learner_fixture(directory.path(), &url, 1_000_000, 1).await?;
        let fixture = claimed_fixture(fixture, directory.path()).await?;
        let checked = async {
            let (input, r1) =
                source(&fixture, &headers, &credential, &url, directory.path()).await?;
            action(Case {
                fixture: &fixture,
                credential: &credential,
                input,
            })
            .await?;
            let state = store::load(&fixture.db, Utc::now()).await?;
            ensure!(state.active_configurations.get(&fixture.tenant) == Some(&r1));
            ensure!(
                server.received_requests().await.context("requests")?.len() == 1,
                "learner replayed provider work"
            );
            tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
            ensure!(fixture.service.host_slots_for_test() == 1);
            ensure!(fixture.service.cleanup.disposition() != Disposition::Ambiguous);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        fixture.service.cleanup.release_gate();
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        stopped??;
        Ok(())
    })
    .await?
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Counts {
    pub resources: i64,
    pub configurations: i64,
    pub publications: i64,
}

pub(super) async fn retained_counts(fixture: &Fixture) -> Result<Counts> {
    let mut counts = Vec::new();
    for (table, key) in [
        ("tenancy_resource_revisions", "owner_id"),
        ("tenancy_configurations", "tenant_id"),
        ("tenancy_operations", "tenant_id"),
    ] {
        let mut query = Query::select();
        query
            .from(Alias::new(table))
            .expr_as(Expr::cust("COUNT(*)"), Alias::new("n"))
            .and_where(Expr::col(Alias::new(key)).eq(fixture.tenant.as_str()));
        if table == "tenancy_operations" {
            query
                .and_where(
                    Expr::col(Alias::new("installation_id")).eq(fixture.installation.as_str()),
                )
                .and_where(Expr::col(Alias::new("operation_id")).like("learn_%"));
        }
        let row = fixture
            .db
            .query_one(fixture.db.get_database_backend().build(&query))
            .await?
            .context("retained count")?;
        counts.push(row.try_get::<i64>("", "n")?);
    }
    Ok(Counts {
        resources: counts[0],
        configurations: counts[1],
        publications: counts[2],
    })
}

pub(super) async fn has_operation(fixture: &Fixture, id: &str) -> Result<bool> {
    let query = Query::select()
        .from(Alias::new("tenancy_operations"))
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("n"))
        .and_where(Expr::col(Alias::new("installation_id")).eq(fixture.installation.as_str()))
        .and_where(Expr::col(Alias::new("operation_id")).eq(id))
        .to_owned();
    let row = fixture
        .db
        .query_one(fixture.db.get_database_backend().build(&query))
        .await?
        .context("operation count")?;
    Ok(row.try_get::<i64>("", "n")? == 1)
}

// Observe H's actual response without the transport waiter's shorter timeout.
// The test still supplies the real per-job deadline to H and waits boundedly.
pub(super) async fn enqueue(
    fixture: &Fixture,
    work: Work,
    deadline: Instant,
) -> Result<tokio::sync::oneshot::Receiver<std::result::Result<Reply, Failure>>> {
    let (reply, receive) = tokio::sync::oneshot::channel();
    let read = fixture.service.reads.clone().try_acquire_owned()?;
    let job = queue::Job {
        work,
        deadline,
        reply,
        read,
        lease: Default::default(),
        test: fixture.service.learner_test.clone(),
    };
    tokio::time::timeout(
        TIMEOUT,
        fixture
            .service
            .commands
            .send(crate::tenancy::actor::Job::Learner(Box::new(job))),
    )
    .await?
    .map_err(|_| anyhow::anyhow!("H closed"))?;
    Ok(receive)
}

pub(super) async fn acquire(
    fixture: &Fixture,
    deadline: Instant,
) -> Result<Arc<learner::source::Acquired>> {
    let Reply::Scanned(page) = queue::request(
        &fixture.service,
        Work::Scan(Default::default()),
        Instant::now() + TIMEOUT,
    )
    .await
    .map_err(failure)?
    else {
        anyhow::bail!("scan");
    };
    let registration = page
        .registrations
        .into_iter()
        .next()
        .context("registration")?;
    let stopped = fixture.service.stopped_learner_source().map_err(failure)?;
    let Reply::Acquired(value) = queue::request(
        &fixture.service,
        Work::Acquire {
            registration,
            stopped,
            deadline,
        },
        Instant::now() + TIMEOUT,
    )
    .await
    .map_err(failure)?
    else {
        anyhow::bail!("acquire");
    };
    Ok(value)
}
pub(super) fn publication(
    acquired: Arc<learner::source::Acquired>,
    shutdown: CancellationToken,
) -> Result<Work> {
    let calculated = learner::source::calculate(&acquired)?;
    ensure!(
        calculated.step.action == crate::core::optimization::controller::ControllerAction::Retreat
    );
    Ok(Work::Publish {
        acquired,
        calculated: Box::new(calculated),
        shutdown,
    })
}

pub(super) fn start(
    fixture: &Fixture,
    shutdown: CancellationToken,
) -> Result<(
    tokio::task::JoinHandle<()>,
    crate::core::evolution::runtime::EvolutionRuntime,
)> {
    let config = crate::config::Config {
        inherit_defaults: false,
        ..Default::default()
    };
    let policies =
        crate::policy_lock::PolicyRuntime::from_document(&config, None, Default::default(), None)?;
    let legacy = crate::core::evolution::runtime::EvolutionRuntime::new(
        fixture.db.clone(),
        Arc::new(crate::config::routing_table::ConfigRoutingTable::from_config(config)),
        policies,
    );
    let worker = crate::core::evolution::scheduler::EvolutionScheduler::start(
        legacy.clone(),
        None,
        Some(fixture.service.clone()),
        crate::daemon_handoff::HandoffGate::default(),
        shutdown,
    )
    .context("owned worker")?;
    Ok((worker, legacy))
}

pub(super) async fn assert_committed(fixture: &Fixture, before: &Counts) -> Result<()> {
    let after = retained_counts(fixture).await?;
    ensure!(
        after.resources == before.resources + 4
            && after.configurations == before.configurations + 1
            && after.publications == before.publications + 1,
        "checkpoint/policy/two routes/config/operation were not atomically retained: {before:?} -> {after:?}"
    );
    let schedules = records(fixture, "owned_learner_schedule_v1").await?;
    ensure!(
        schedules.len() == 1
            && schedules[0]["next_sequence"] == 2
            && schedules[0]["completed_fence"].is_string()
    );
    ensure!(fixture.service.cleanup.disposition() == Disposition::Committed);
    let stored = store::load(&fixture.db, Utc::now()).await?;
    let published = fixture
        .service
        .snapshot
        .read()
        .map_err(|_| anyhow::anyhow!("snapshot lock"))?;
    ensure!(
        published.revision == stored.revision && published.operation_ids == stored.operation_ids,
        "known committed snapshot was not published before delivery refusal"
    );
    Ok(())
}
