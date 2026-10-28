//! Actual OwnedDatabase readiness and stopped-native repair, consumed by the
//! registered SDK gauges and the same bounded OTLP receiver as the M5 campaign.
use super::*;
use crate::telemetry::receipt_latency::capacity::ReadError;
use crate::tenancy::tests::execution::receipts::latency::{
    callback_schedule::{capacity_points, collect_until},
    collector::Collector,
    exporter, flush,
};
use crate::tenancy::{BootstrapGate, StartupTest, transaction::Disposition};
use std::sync::atomic::{AtomicBool, AtomicUsize};

#[tokio::test]
#[ignore = "isolated actual owned startup and native repair SDK consumers; select this parent"]
async fn owned_startup_and_native_repair_capacity_consumers() -> Result<()> {
    for mode in [
        "observation-startup",
        "observation-refused-sealed",
        "observation-refused-bound",
        "observation-repair-commit",
        "observation-repair-rollback",
        "observation-repair-unknown",
    ] {
        let root = tempfile::tempdir()?;
        let mut child = Child::start("companion::observation::controller", root.path(), mode)?;
        let observed = child.line("native-observation-checked").await;
        let reaped = child.reap(observed.is_err()).await;
        observed?;
        reaped?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "internal observation controller; select public parent only"]
async fn controller() -> Result<()> {
    let Some(root) = std::env::var_os(ROOT) else {
        return Ok(());
    };
    let root = Path::new(&root);
    let mode = std::env::var(MODE)?;
    match mode.as_str() {
        "observation-startup" => startup(root).await?,
        "observation-refused-sealed" | "observation-refused-bound" => refusal(root, &mode).await?,
        "observation-repair-commit"
        | "observation-repair-rollback"
        | "observation-repair-unknown" => repair(root, &mode).await?,
        _ => anyhow::bail!("unknown observation mode"),
    }
    signal("native-observation-checked")
}

fn config() -> crate::config::tenancy::TenancyConfig {
    crate::config::tenancy::TenancyConfig {
        enabled: true,
        wrapping_key_env: Some(KEY.into()),
        ..Default::default()
    }
}
async fn database(root: &Path) -> Result<crate::db::OwnedDatabase> {
    let database =
        crate::db::connect_owned_tenancy(&format!("sqlite://{}", root.join("owned.db").display()))
            .await?;
    crate::db::run_migrations(database.database()).await?;
    Ok(database)
}

async fn startup(root: &Path) -> Result<()> {
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 3_600_000, None)?;
    let observation = sink.capacity_for_test().context("enabled observer")?;
    let callbacks = Arc::new(AtomicUsize::new(0));
    let calls = callbacks.clone();
    ensure!(
        observation.schedule_collection_for_test(Box::new(move |collect| {
            calls.fetch_add(1, Ordering::SeqCst);
            collect()
        }))
    );
    let owned = database(root).await?;
    let db = owned.database().clone();
    let (attached, attachment) = tokio::sync::oneshot::channel();
    let bound_under_lock = Arc::new(AtomicBool::new(false));
    let witnessed = bound_under_lock.clone();
    let observed = observation.clone();
    let hooks = StartupTest {
        bootstrap: Some(BootstrapGate {
            phase: Phase::Committing,
            attached,
        }),
        bind: Some(Box::new(move |service, bind| {
            service.cleanup.ensure_clean()?;
            // The Worker's ACK has arrived. Exercise the actual binder while
            // the actual Admission mutex cannot be read, on a multithread runtime.
            // This fails deterministically if Service rereads instead of using ACK data.
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            loop {
                match service.admission.with_observation_lock_for_test(|| {
                    ensure!(service.admission.observed_capacity() == Err(ReadError::Unavailable));
                    bind()?;
                    let [initial, current] = observed.collect();
                    let value = initial
                        .context("handoff initial")?
                        .1
                        .map_err(|_| anyhow::anyhow!("handoff invalid"))?;
                    ensure!(
                        value.accounted == 0 && value.active == 0 && value.potential_unknown == 0
                    );
                    ensure!(current == Some(("current", Err(ReadError::Unavailable))));
                    witnessed.store(true, Ordering::SeqCst);
                    Ok(())
                }) {
                    Ok(result) => return result,
                    Err(_) if std::time::Instant::now() < deadline => std::thread::yield_now(),
                    Err(_) => anyhow::bail!("startup scheduling mutex unavailable"),
                }
            }
        })),
    };
    let startup_sink = sink.clone();
    let opening = tokio::spawn(async move {
        // Same consuming constructor as open_owned_profile_observed; only the
        // per-instance test scheduler differs. It receives the real OwnedDatabase.
        Service::open_owned_profile_observed_startup(
            owned,
            &config(),
            &crate::config::ContinuationConfig::default(),
            startup_sink,
            Some(hooks),
        )
        .await
    });
    let (proof, gate) = tokio::time::timeout(TIMEOUT, attachment).await??;
    let paused = async {
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        ensure!(proof.disposition() == Disposition::Pending && !opening.is_finished());
        locked(root)?;
        collect_until(
            &exporter,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await?;
        ensure!(
            callbacks.load(Ordering::SeqCst) >= 2,
            "actual SDK callbacks were not invoked"
        );
        ensure!(
            observation.collect() == [None, None],
            "initial escaped bootstrap ACK"
        );
        let view = collector.view()?;
        ensure!(view.immutable.is_empty() && view.current.is_empty());
        Ok::<_, anyhow::Error>(())
    }
    .await;
    proof.release_gate();
    let opened = tokio::time::timeout(TIMEOUT, opening).await??;
    let service = opened?;
    let checked = async {
        paused?;
        ensure!(bound_under_lock.load(Ordering::SeqCst));
        proof.ensure_clean()?;
        collect_until(&exporter, tokio::time::Instant::now() + TIMEOUT).await?;
        let points = capacity_points(
            &collector.packet(
                collector
                    .accepted_http()
                    .checked_sub(1)
                    .context("actual SDK packet")?,
            )?,
        )?;
        for checkpoint in ["initial", "current"] {
            ensure!(points.get(&format!("{checkpoint}:available")) == Some(&1));
            for field in [
                "completion_reserved",
                "accounted",
                "potential_unknown",
                "active",
            ] {
                ensure!(points.get(&format!("{checkpoint}:{field}")) == Some(&0));
            }
        }
        ensure!(points.len() == 22);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = service.shutdown().await;
    let _timing_complete = sink.seal(closed.is_ok()); // No selected execution in this startup case.
    drop(service);
    let pool_closed = db.close().await;
    let exported = flush(&exporter).await;
    let receiver_closed = collector.close().await;
    checked?;
    closed?;
    pool_closed?;
    ensure!(exported?);
    receiver_closed?;
    wait_file(root).await?;
    // The default-off constructor consumes a fresh acquisition of the same file.
    let database = database(root).await?;
    let db = database.database().clone();
    let disabled = Service::open_owned_profile(
        database,
        &config(),
        &crate::config::ContinuationConfig::default(),
    )
    .await?;
    ensure!(!disabled.receipt_observer().enabled());
    disabled.shutdown().await?;
    drop(disabled);
    db.close().await?;
    wait_file(root).await
}

async fn refusal(root: &Path, mode: &str) -> Result<()> {
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 3_600_000, None)?;
    if mode == "observation-refused-bound" {
        let first = database(root).await?;
        let db = first.database().clone();
        let service = Service::open_owned_profile_observed(
            first,
            &config(),
            &crate::config::ContinuationConfig::default(),
            sink.clone(),
        )
        .await?;
        service.shutdown().await?;
        drop(service);
        db.close().await?;
        wait_file(root).await?;
    } else {
        ensure!(!sink.seal(false));
    }
    let before = sink.capacity_for_test().context("observer")?.collect()[0];
    let next = database(root).await?;
    let db = next.database().clone();
    let opened = Service::open_owned_profile_observed(
        next,
        &config(),
        &crate::config::ContinuationConfig::default(),
        sink.clone(),
    )
    .await;
    if let Ok(service) = opened {
        service.shutdown().await?;
        anyhow::bail!("genuine observer refusal returned a Service");
    }
    let error = opened.err().context("expected binding refusal")?;
    ensure!(
        error.to_string().contains("already bound or sealed"),
        "unexpected refusal: {error}"
    );
    ensure!(sink.capacity_for_test().context("observer")?.collect()[0] == before);
    // No returned Service retains the claim; closing the last real pool owner
    // must permit another real acquisition. The old SDK remains alive here.
    db.close().await?;
    wait_file(root).await?;
    let next = database(root).await?;
    let db = next.database().clone();
    let fresh = Service::open_owned_profile(
        next,
        &config(),
        &crate::config::ContinuationConfig::default(),
    )
    .await?;
    fresh.cleanup.ensure_clean()?;
    fresh.shutdown().await?;
    drop(fresh);
    db.close().await?;
    let _timing_complete = sink.seal(false);
    let exported = flush(&exporter).await;
    let closed = collector.close().await;
    ensure!(exported?);
    closed?;
    wait_file(root).await
}

async fn repair(root: &Path, mode: &str) -> Result<()> {
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 3_600_000, None)?;
    let stopped = Stopped::produce_observed(root, mode, sink.clone()).await?;
    let unknown = mode == "observation-repair-unknown";
    let rollback = mode == "observation-repair-rollback";
    let result = repair_held(&stopped, root, &collector, &exporter, rollback, unknown).await;
    stopped.bound.fixture.service.cleanup.release_gate();
    let shutdown = if unknown {
        Ok(())
    } else {
        stopped.bound.fixture.service.shutdown().await
    };
    // Freeze while the actual Admission owner is still retained, then prove the
    // SDK can export after ordinary Service teardown. Unknown never gets clean input.
    let timing_complete = sink.seal(!unknown && shutdown.is_ok());
    let stopped_result = if unknown {
        // A failed consuming COMMIT has real quarantined H custody. The isolated
        // controller exits with that owner; do not pretend shutdown acknowledged it.
        let Stopped {
            generation,
            mut choice,
            bound,
            ..
        } = stopped;
        choice.release();
        let choice_closed = choice.finish().await;
        let generation_closed = generation.finish().await;
        drop(bound);
        result.and(choice_closed).and(generation_closed)
    } else {
        stopped.close(result.and(shutdown)).await
    };
    ensure!(
        !timing_complete,
        "repair is not a successful selected execution cohort"
    );
    let exported = flush(&exporter).await;
    let view = collector.view();
    let closed = collector.close().await;
    stopped_result?;
    ensure!(exported?);
    let view = view?;
    if unknown {
        ensure!(view.immutable.get("final:available") == Some(&0) && view.frozen.is_none());
    } else {
        let frozen = view
            .frozen
            .context("single complete SDK initial/final batch")?;
        ensure!(frozen.get("final:completion_reserved") == Some(&0));
        ensure!(frozen.get("final:potential_unknown") == Some(&0));
        ensure!(frozen.get("final:live_unknown") == Some(&if rollback { 0 } else { 2 }));
        ensure!(frozen.get("final:active") == Some(&0));
    }
    closed
}

async fn repair_held(
    stopped: &Stopped,
    root: &Path,
    collector: &Collector,
    exporter: &Arc<crate::telemetry::exporter::OtelExporter>,
    rollback: bool,
    unknown: bool,
) -> Result<()> {
    let bound = &stopped.bound;
    let before = payload(&bound.fixture.db).await?;
    ensure!(terminals(&bound.fixture.db).await?.is_empty());
    let journal = fixture::journal(bound).await?;
    let child = journal["owned_choices"]
        .as_object()
        .context("real intent")?
        .values()
        .next()
        .context("real child link")?["child_execution"]
        .as_str()
        .context("child execution")?;
    bound
        .fixture
        .service
        .cleanup
        .observe_receipt_for_test(Key {
            installation: bound.fixture.installation.clone(),
            tenant: bound.fixture.tenant.clone(),
            execution: child.try_into()?,
        })?;
    if rollback {
        bound.fixture.db.execute_unprepared("CREATE TRIGGER observed_repair_rollback BEFORE UPDATE ON bro_managed_sessions BEGIN SELECT RAISE(ABORT,'observed repair rollback'); END").await?;
    } else if unknown {
        for sql in [
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE observed_parent(id INTEGER PRIMARY KEY)",
            "CREATE TABLE observed_child(id INTEGER REFERENCES observed_parent(id) DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TRIGGER observed_repair_commit AFTER UPDATE ON bro_managed_sessions BEGIN INSERT INTO observed_child(id) VALUES(1); END",
        ] {
            bound.fixture.db.execute_unprepared(sql).await?;
        }
    }
    let resources = bound
        .fixture
        .service
        .hold_host(crate::tenancy::hosts::HostBoundary::Resources)?;
    let app = Arc::new(
        host_app(
            &bound.fixture,
            &stopped.generation.url,
            Arc::new(HostSpies::default()),
        )?
        .with_tenancy(bound.fixture.service.clone()),
    );
    let headers = bound.headers.clone();
    let workspace = root.join("workspace");
    let opening = tokio::spawn(async move {
        NativeSession::open_authenticated(
            app,
            headers,
            "choice-session",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
    });
    let held = async {
        tokio::time::timeout(TIMEOUT, resources.entered.notified()).await?;
        let gate = bound.fixture.service.cleanup.hold(if rollback {
            Phase::RollingBack
        } else {
            Phase::Committing
        });
        resources.release();
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        ensure!(bound.fixture.service.cleanup.disposition() == Disposition::Pending);
        let (charge, _, _) = bound
            .fixture
            .service
            .admission
            .receipt_capacity_for_test()
            .map_err(|e| anyhow::anyhow!("repair accounting {e:?}"))?;
        ensure!(charge > 2 * 16 * 1024 && charge < 8 * 1024 * 1024);
        // This is the actual consuming repair transaction after real inserts.
        // Admission is not locked during SDK collection; rejection is an invariant bug.
        collect_until(
            exporter,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await?;
        let points = capacity_points(&collector.packet(0)?)?;
        for (field, value) in [
            ("completion_reserved", charge),
            ("potential_unknown", 2),
            ("active", 0),
            ("live_unknown", 0),
            ("accounted", 0),
            ("available", 1),
        ] {
            ensure!(
                points.get(&format!("current:{field}")) == Some(&value),
                "missing exact repair field {field}"
            );
        }
        ensure!(points.get("initial:completion_reserved") == Some(&0) && points.len() == 22);
        let trace = bound.fixture.service.cleanup.receipt_trace_for_test()?;
        ensure!(!trace.overflow);
        let fact = trace.facts[0]
            .as_ref()
            .context("actual repair receipt SQL")?;
        ensure!(fact.inserted == Some(true) && fact.store.is_some_and(|r| r.ok));
        ensure!(
            fact.commit.is_none() && fact.rollback.is_none(),
            "hold was after settlement"
        );
        locked(root)?;
        Ok::<_, anyhow::Error>(charge)
    }
    .await;
    resources.release();
    bound.fixture.service.cleanup.release_gate();
    let opened = tokio::time::timeout(TIMEOUT, opening).await??;
    // Always release/join the real opening owner before propagating a held-check failure.
    if rollback || unknown {
        if let Ok(session) = opened {
            session.close().await?;
            anyhow::bail!("faulted repair returned a native handle");
        }
    } else {
        opened?.close().await?;
    }
    let charge = held?;
    let trace = bound.fixture.service.cleanup.receipt_trace_for_test()?;
    let fact = trace.facts[0].as_ref().context("actual settlement fact")?;
    if unknown {
        ensure!(
            fact.commit
                .is_some_and(|r| !r.ok && r.sqlite_code == Some(787))
                && fact.rollback.is_none()
        );
        ensure!(bound.fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        ensure!(bound.fixture.service.cleanup.ensure_clean().is_err());
        locked(root)?;
    } else if rollback {
        ensure!(
            fact.native_checkpoint
                .is_some_and(|r| !r.ok && r.sqlite_code == Some(1811))
        );
        ensure!(fact.rollback.is_some_and(|r| r.ok) && fact.commit.is_none());
        bound.fixture.service.cleanup.ensure_clean()?;
    } else {
        ensure!(fact.commit.is_some_and(|r| r.ok) && fact.rollback.is_none());
        bound.fixture.service.cleanup.ensure_clean()?;
    }
    let reader = crate::db::connect(&format!(
        "sqlite://{}?mode=ro",
        root.join("owned.db").display()
    ))
    .await?;
    let readback = async {
        if rollback || unknown {
            ensure!(terminals(&reader).await?.is_empty() && payload(&reader).await? == before);
            Ok::<u64, anyhow::Error>(0)
        } else {
            let rows = terminals(&reader).await?;
            ensure!(rows.len() == 2);
            let row = reader.query_one(Statement::from_string(DatabaseBackend::Sqlite,
                "SELECT COALESCE(SUM(byte_count),0) AS bytes,COUNT(*) AS rows FROM tenancy_execution_receipts"))
                .await?.context("actual durable repair bytes")?;
            let bytes = u64::try_from(row.try_get::<i64>("", "bytes")?)?;
            let rows = u64::try_from(row.try_get::<i64>("", "rows")?)?;
            ensure!(bytes + rows * 16 * 1024 == charge, "reserved charge disagrees with actual committed rows");
            Ok(charge)
        }
    }.await;
    let reader_closed = reader.close().await;
    let accounted = readback?;
    reader_closed?;
    collect_until(exporter, tokio::time::Instant::now() + TIMEOUT).await?;
    let points = capacity_points(&collector.packet(1)?)?;
    for (field, value) in [
        ("completion_reserved", if unknown { charge } else { 0 }),
        ("potential_unknown", if unknown { 2 } else { 0 }),
        ("active", 0),
        ("live_unknown", if rollback || unknown { 0 } else { 2 }),
        ("accounted", accounted),
        ("available", 1),
    ] {
        ensure!(
            points.get(&format!("current:{field}")) == Some(&value),
            "settled repair field {field}"
        );
    }
    ensure!(
        stopped.generation.calls() == 0 && stopped.choice.calls() == 1,
        "repair redispatched provider work"
    );
    Ok(())
}
