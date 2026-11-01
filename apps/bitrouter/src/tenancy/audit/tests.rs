use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Notify;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Read,
    Rollback,
    Close,
}
pub(super) struct Gate {
    entered: Notify,
    release: CancellationToken,
    body: Mutex<Option<Result<(), ErrorCode>>>,
}
#[derive(Default)]
pub(super) struct Hooks {
    gate: Mutex<Option<(Step, Arc<Gate>)>>,
    wrong_digest: AtomicBool,
    digest_fault_observed: AtomicBool,
}
impl Hooks {
    pub(super) fn receipt_sql(&self, sql: &str) -> String {
        let Some(rest) = sql.strip_prefix("SELECT * FROM \"tenancy_execution_receipts\" ") else {
            return sql.to_owned();
        };
        if !self.wrong_digest.swap(false, Ordering::SeqCst) {
            return sql.to_owned();
        }
        // Inject one wrong digest at the real SQL/decoder boundary after a
        // fully validated startup. All other columns and the bounded point
        // predicate remain original. Never mutate SQLite's immutable file or
        // synthesize the decoder's error/fatal classification.
        format!(
            "SELECT installation_id,tenant_id,execution_id,principal_id,correlation_id,receipt_json,CASE WHEN content_digest='{}' THEN '{}' ELSE '{}' END AS content_digest,content_digest AS fixture_original_digest,disposition_json,byte_count,unknown_outcome,recorded_at FROM \"tenancy_execution_receipts\" {rest}",
            "0".repeat(64),
            "1".repeat(64),
            "0".repeat(64)
        )
    }
    pub(super) fn observe_receipt_row(&self, row: &sea_orm::QueryResult) {
        if let (Ok(original), Ok(wrong)) = (
            row.try_get::<String>("", "fixture_original_digest"),
            row.try_get::<String>("", "content_digest"),
        ) && original.len() == 64
            && wrong.len() == 64
            && original != wrong
        {
            // A SQL error cannot satisfy the fault witness: a real changed
            // digest cell must reach the unchanged receipt decoder.
            self.digest_fault_observed.store(true, Ordering::SeqCst);
        }
    }
    fn hold(&self, step: Step) -> Result<Arc<Gate>> {
        let gate = Arc::new(Gate {
            entered: Notify::new(),
            release: CancellationToken::new(),
            body: Mutex::new(None),
        });
        let previous = self
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("audit hook poisoned"))?
            .replace((step, gate.clone()));
        ensure!(previous.is_none());
        Ok(gate)
    }
    pub(super) fn observe_rollback_body(&self, body: &Result<Vec<u8>, Failure>) {
        let slots = self
            .gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some((Step::Rollback, gate)) = slots.as_ref() {
            *gate
                .body
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) =
                Some(body.as_ref().map(|_| ()).map_err(|failure| failure.0.code));
        }
    }
    pub(super) async fn hit(&self, step: Step) {
        let gate = {
            let mut slot = match self.gate.lock() {
                Ok(slot) => slot,
                Err(error) => error.into_inner(),
            };
            if slot.as_ref().is_some_and(|(held, _)| *held == step) {
                slot.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.cancelled().await;
        }
    }
}
const WAIT: Duration = Duration::from_secs(10);

// Verify SQLite's actual storage, not just the existence of a sidecar lease.
// An in-memory connection's VACUUM INTO may retain SQLITE_OPEN_MEMORY.
pub(in crate::tenancy) async fn assert_file_database(
    db: &sea_orm::DatabaseConnection,
    source: &Path,
) -> Result<()> {
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA database_list",
        ))
        .await?;
    let mut main = None;
    for row in rows {
        if row.try_get::<String>("", "name")? == "main" {
            ensure!(main.is_none(), "duplicate SQLite main database");
            main = Some(row.try_get::<String>("", "file")?);
        }
    }
    let main = main.context("SQLite main database missing")?;
    ensure!(
        !main.is_empty() && Path::new(&main).canonicalize()? == source.canonicalize()?,
        "SQLite main is not the fixture's leased file"
    );
    let metadata = std::fs::metadata(source)?;
    ensure!(
        metadata.is_file() && metadata.len() > 0,
        "authority file was not materialized"
    );
    Ok(())
}

async fn bundle(directory: &Path) -> Result<(PathBuf, String)> {
    use std::os::unix::fs::PermissionsExt;
    let live_source = directory.join("authority.sqlite");
    let owner = crate::tenancy::ownership::acquire_path(&live_source, true)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    let source = directory.join("retired.sqlite");
    crate::db::run_migrations(&db).await?;
    let initialized = super::super::store::load(&db, chrono::Utc::now()).await?;
    // Preserve a real stored latch without claiming a production-capacity test.
    db.execute_unprepared("UPDATE tenancy_installation SET saturated=1")
        .await?;
    assert_file_database(&db, &live_source).await?;
    let persisted = super::super::store::load_existing(&db, chrono::Utc::now()).await?;
    ensure!(persisted.installation == initialized.installation && persisted.saturated);
    ensure!(crate::tenancy::ownership::acquire_path(&live_source, false).is_err());
    std::fs::File::create(&source)?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "VACUUM INTO ?",
        [source.to_str().context("source path")?.into()],
    ))
    .await?;
    db.close().await?;
    drop(owner);
    // Pool callbacks can finish dropping after close returns. Observe actual
    // lease release; elapsed time alone is never a cleanup witness.
    let released = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(lease) = crate::tenancy::ownership::acquire_path(&live_source, false) {
                break lease;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    drop(released);
    ensure!(
        std::fs::metadata(&source)?.len() > 0,
        "archive copy was not materialized"
    );
    let bundle = directory.join("bundle");
    let report = {
        let report = crate::offline::snapshot(&source, &bundle).await?;
        serde_json::to_value(&report as &dyn crate::output::CliReport)?
    };
    Ok((
        bundle,
        report["evidence_digest"].as_str().context("digest")?.into(),
    ))
}
fn inventory(directory: &Path) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let mut result = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        result.insert(
            entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("name"))?,
            std::fs::read(entry.path())?,
        );
    }
    Ok(result)
}

#[tokio::test]
async fn existing_only_decoder_never_creates_identity_or_repairs_missing_counters() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    ensure!(
        super::super::store::load_existing(&db, chrono::Utc::now())
            .await
            .is_err()
    );
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM tenancy_installation",
        ))
        .await?
        .context("count")?;
    ensure!(row.try_get::<i64>("", "n")? == 0);
    // Ordinary live startup retains its initializer; archive uses the other API.
    let first = super::super::store::load(&db, chrono::Utc::now()).await?;
    let second = super::super::store::load_existing(&db, chrono::Utc::now()).await?;
    ensure!(first.installation == second.installation && second.operation_ids == 0);
    db.execute_unprepared("UPDATE tenancy_installation SET operation_ids=1")
        .await?;
    ensure!(
        super::super::store::load_existing(&db, chrono::Utc::now())
            .await
            .is_err()
    );
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT operation_ids FROM tenancy_installation",
        ))
        .await?
        .context("counter")?;
    ensure!(row.try_get::<i64>("", "operation_ids")? == 1);
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn archive_projection_requires_anchor_and_stays_outside_exact_bundle() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (bundle, digest) = bundle(directory.path()).await?;
    let before = inventory(&bundle)?;
    let stop = CancellationToken::new();
    let _stop_on_drop = stop.clone().drop_guard();
    ensure!(
        crate::offline::audit::open(&bundle, &bundle.join("projection"), &digest, stop.clone())
            .await
            .is_err()
    );
    let wrong = directory.path().join("wrong-anchor");
    ensure!(
        crate::offline::audit::open(&bundle, &wrong, &"0".repeat(64), stop.clone())
            .await
            .is_err()
    );
    ensure!(!wrong.exists() && inventory(&bundle)? == before);
    for number in 0..2 {
        let projection = directory.path().join(format!("projection-{number}"));
        let reader =
            crate::offline::audit::open(&bundle, &projection, &digest, stop.child_token()).await?;
        ensure!(reader.state.saturated && reader.state.operation_ids == 0);
        let path = projection.join("audit.sqlite");
        ensure!(crate::tenancy::ownership::acquire_path(&path, false).is_err());
        reader.shutdown().await?;
        drop(crate::tenancy::ownership::acquire_path(&path, false)?);
        ensure!(inventory(&bundle)? == before);
    }
    Ok(())
}
#[tokio::test]
async fn cancelled_read_and_shutdown_keep_actual_connection_until_rollback_and_close() -> Result<()>
{
    for step in [Step::Read, Step::Rollback] {
        let directory = tempfile::tempdir()?;
        let (bundle, digest) = bundle(directory.path()).await?;
        let before = inventory(&bundle)?;
        let projection = directory.path().join("projection");
        let reader =
            crate::offline::audit::open(&bundle, &projection, &digest, CancellationToken::new())
                .await?;
        let _stop_on_drop = reader.stop.clone().drop_guard();
        let gate = reader.hooks.hold(step)?;
        let release = gate.release.clone().drop_guard();
        let caller = Caller::Installation(
            crate::tenancy::operator_principal("audit")
                .map_err(|failure| anyhow::anyhow!("audit operator: {:?}", failure.0.code))?,
        );
        let call = {
            let reader = reader.clone();
            tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                reader
                    .read(
                        caller,
                        "tenant".try_into()?,
                        read::Request::One("missing".try_into()?),
                    )
                    .await
                    .map_err(|failure| anyhow::anyhow!("{:?}", failure.0.code))
            }))
        };
        tokio::time::timeout(WAIT, gate.entered.notified()).await?;
        call.abort();
        ensure!(call.await.is_err());
        let path = projection.join("audit.sqlite");
        ensure!(reader.reads.available_permits() == READS - 1);
        ensure!(crate::tenancy::ownership::acquire_path(&path, false).is_err());
        let close = reader.hooks.hold(Step::Close)?;
        let release_close = close.release.clone().drop_guard();
        let mut shutdown = Box::pin(reader.shutdown());
        ensure!(futures::poll!(&mut shutdown).is_pending());
        ensure!(crate::tenancy::ownership::acquire_path(&path, false).is_err());
        gate.release.cancel();
        tokio::time::timeout(WAIT, close.entered.notified()).await?;
        ensure!(futures::poll!(&mut shutdown).is_pending());
        ensure!(crate::tenancy::ownership::acquire_path(&path, false).is_err());
        close.release.cancel();
        tokio::time::timeout(WAIT, shutdown).await??;
        drop(crate::tenancy::ownership::acquire_path(&path, false)?);
        ensure!(inventory(&bundle)? == before);
        drop((release, release_close));
    }
    Ok(())
}

#[tokio::test]
async fn socket_queue_is_bounded_and_disconnect_cannot_release_the_sql_owner() -> Result<()> {
    let mut protocols = vec![false];
    if cfg!(feature = "http2") {
        protocols.push(true);
    }
    for h2 in protocols {
        let directory = tempfile::tempdir()?;
        let (bundle, digest) = bundle(directory.path()).await?;
        let before = inventory(&bundle)?;
        let projection = directory.path().join("projection");
        let stop = CancellationToken::new();
        let _stop_on_drop = stop.clone().drop_guard();
        let reader =
            crate::offline::audit::open(&bundle, &projection, &digest, stop.clone()).await?;
        let gate = reader.hooks.hold(Step::Read)?;
        let _release_on_drop = gate.release.clone().drop_guard();
        let token = "actual-audit-socket-token-at-least-32-bytes";
        let controls = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: "audit".into(),
                    token_env: "AUDIT_TOKEN".into(),
                    scopes: vec![bitrouter_ai::control::ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some(token.into()),
        )?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let uri = format!(
            "http://{}/control/v1/tenants/tenant/executions/missing",
            listener.local_addr()?
        );
        let server = {
            let reader = reader.clone();
            tokio::spawn(async move { reader.serve(listener, controls).await })
        };
        let builder = reqwest::Client::builder().no_proxy().timeout(WAIT);
        #[cfg(feature = "http2")]
        let builder = if h2 {
            builder.http2_prior_knowledge()
        } else {
            builder.http1_only()
        };
        #[cfg(not(feature = "http2"))]
        ensure!(!h2);
        let client = builder.build()?;
        let request = || {
            client
                .get(&uri)
                .bearer_auth(token)
                .header("bitrouter-installation-id", reader.installation().as_str())
        };
        let mut calls = Vec::new();
        for _ in 0..READS {
            calls.push(tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
                request().send(),
            )));
        }
        let checked = async {
            tokio::time::timeout(WAIT, gate.entered.notified()).await?;
            tokio::time::timeout(WAIT, async {
                while reader.reads.available_permits() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            let full = request().send().await?;
            ensure!(full.status() == reqwest::StatusCode::TOO_MANY_REQUESTS);
            ensure!(
                full.version()
                    == if h2 {
                        reqwest::Version::HTTP_2
                    } else {
                        reqwest::Version::HTTP_11
                    }
            );
            ensure!(
                full.headers()
                    .get("cache-control")
                    .is_some_and(|value| value == "no-store")
            );
            for call in &calls {
                call.abort();
            }
            ensure!(
                crate::tenancy::ownership::acquire_path(&projection.join("audit.sqlite"), false)
                    .is_err()
            );
            let close = reader.hooks.hold(Step::Close)?;
            let _release_close = close.release.clone().drop_guard();
            stop.cancel();
            ensure!(!server.is_finished());
            gate.release.cancel();
            tokio::time::timeout(WAIT, close.entered.notified()).await?;
            ensure!(
                !server.is_finished()
                    && crate::tenancy::ownership::acquire_path(
                        &projection.join("audit.sqlite"),
                        false
                    )
                    .is_err()
            );
            close.release.cancel();
            Ok::<_, anyhow::Error>(())
        }
        .await;
        stop.cancel();
        gate.release.cancel();
        drop(calls);
        let ended = tokio::time::timeout(WAIT, server).await;
        let closed = reader.shutdown().await;
        checked?;
        ended???;
        closed?;
        drop(crate::tenancy::ownership::acquire_path(
            &projection.join("audit.sqlite"),
            false,
        )?);
        ensure!(inventory(&bundle)? == before);
    }
    Ok(())
}

#[tokio::test]
async fn encoded_reply_slices_retain_read_slots_after_body_owner_disappears() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (bundle, digest) = bundle(directory.path()).await?;
    let before = inventory(&bundle)?;
    let projection = directory.path().join("projection");
    let stop = CancellationToken::new();
    let _stop_on_drop = stop.clone().drop_guard();
    let reader = crate::offline::audit::open(&bundle, &projection, &digest, stop).await?;
    let checked = async {
        let caller = Caller::Installation(
            crate::tenancy::operator_principal("audit")
                .map_err(|failure| anyhow::anyhow!("audit operator: {:?}", failure.0.code))?,
        );
        let tenant: TenantId = "tenant".try_into()?;
        let correlation: bitrouter_ai::tenancy::identity::CorrelationId =
            "correlation".try_into()?;
        let request = || read::Request::Page {
            correlation: correlation.clone(),
            after: None,
            limit: 1,
        };
        let mut slices = Vec::new();
        for _ in 0..READS {
            let reply = reader
                .read(caller.clone(), tenant.clone(), request())
                .await
                .map_err(|failure| anyhow::anyhow!("{:?}", failure.0.code))?;
            let bytes = axum::body::Bytes::from_owner(reply);
            let slice = bytes.slice(..1);
            drop(bytes);
            slices.push(slice);
        }
        ensure!(reader.reads.available_permits() == 0);
        ensure!(
            matches!(reader.read(caller.clone(), tenant.clone(), request()).await,
            Err(failure) if failure.0.code == ErrorCode::CapacityExceeded)
        );
        drop(slices.pop());
        ensure!(reader.reads.available_permits() == 1);
        let reply = reader
            .read(caller, tenant, request())
            .await
            .map_err(|failure| anyhow::anyhow!("{:?}", failure.0.code))?;
        ensure!(reader.reads.available_permits() == 0);
        drop(reply);
        drop(slices);
        ensure!(reader.reads.available_permits() == READS);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = reader.shutdown().await;
    checked?;
    closed?;
    drop(crate::tenancy::ownership::acquire_path(
        &projection.join("audit.sqlite"),
        false,
    )?);
    ensure!(inventory(&bundle)? == before);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cleanup_observer_timeout_does_not_replace_close_or_release_canonical_owner() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let (bundle, digest) = bundle(directory.path()).await?;
    let before = inventory(&bundle)?;
    let projection = directory.path().join("projection");
    let reader =
        crate::offline::audit::open(&bundle, &projection, &digest, CancellationToken::new())
            .await?;
    let _stop_on_drop = reader.stop.clone().drop_guard();
    let gate = reader.hooks.hold(Step::Close)?;
    let _release_on_drop = gate.release.clone().drop_guard();
    let mut shutdown = Box::pin(reader.shutdown());
    ensure!(futures::poll!(&mut shutdown).is_pending());
    tokio::time::timeout(WAIT, gate.entered.notified()).await?;
    // Time only expires the observer. The real worker/connection is still
    // retained immediately before its close call, never replaced by a proof.
    tokio::time::pause();
    tokio::time::advance(CLEANUP + Duration::from_secs(1)).await;
    let expired = shutdown.await;
    tokio::time::resume();
    ensure!(expired.is_err());
    ensure!(
        crate::tenancy::ownership::acquire_path(&projection.join("audit.sqlite"), false).is_err()
    );
    gate.release.cancel();
    tokio::time::timeout(WAIT, reader.shutdown()).await??;
    drop(crate::tenancy::ownership::acquire_path(
        &projection.join("audit.sqlite"),
        false,
    )?);
    ensure!(inventory(&bundle)? == before);
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(in crate::tenancy) enum WaiterEnd {
    Cancel,
    Deadline,
}

/// Called only by the real provider-receipt fixtures. The error observation is
/// captured from the actual point-read body before holding its actual rollback.
pub(in crate::tenancy) async fn held_receipt_read(
    bundle: &Path,
    digest: &str,
    receipt: &bitrouter_ai::tenancy::execution::ExecutionReceipt,
    ending: WaiterEnd,
    fatal: bool,
) -> Result<()> {
    let before = inventory(bundle)?;
    let projection = bundle
        .parent()
        .context("fixture directory")?
        .join(format!("projection-{ending:?}"));
    let reader =
        crate::offline::audit::open(bundle, &projection, digest, CancellationToken::new()).await?;
    let _stop_on_drop = reader.stop.clone().drop_guard();
    // Startup has already verified the receipt and bound evidence. Activate
    // only the point-read digest fault, preserving the original close fence.
    reader.hooks.wrong_digest.store(fatal, Ordering::SeqCst);
    let rollback = reader.hooks.hold(Step::Rollback)?;
    let _release_rollback = rollback.release.clone().drop_guard();
    let caller = Caller::Installation(
        crate::tenancy::operator_principal("audit")
            .map_err(|failure| anyhow::anyhow!("audit operator: {:?}", failure.0.code))?,
    );
    let (tenant, id) = (receipt.tenant_id.clone(), receipt.execution_id.clone());
    let call = {
        let (reader, caller, tenant, id) =
            (reader.clone(), caller.clone(), tenant.clone(), id.clone());
        tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            reader
                .read(caller, tenant, read::Request::One(id))
                .await
                .map_err(|failure| failure.0.code)
        }))
    };
    let checked = async {
        tokio::time::timeout(WAIT, rollback.entered.notified()).await?;
        ensure!(
            reader.hooks.digest_fault_observed.load(Ordering::SeqCst) == fatal,
            "point read did not exercise the intended digest fault"
        );
        let observed = (*rollback
            .body
            .lock()
            .map_err(|_| anyhow::anyhow!("read observation poisoned"))?)
        .context("actual read body observation")?;
        ensure!(
            observed
                == if fatal {
                    Err(ErrorCode::StorageUnavailable)
                } else {
                    Ok(())
                }
        );
        ensure!(!reader.stop.is_cancelled() && reader.reads.available_permits() == READS - 1);
        match ending {
            WaiterEnd::Cancel => {
                call.abort();
                ensure!(matches!(call.await, Err(error) if error.is_cancelled()));
            }
            WaiterEnd::Deadline => {
                // Expire the actual read waiter while its actual rollback is
                // held. Virtual time never stands in for SQL cleanup evidence.
                tokio::time::pause();
                tokio::time::advance(crate::tenancy::MANAGEMENT_TIMEOUT + Duration::from_secs(1))
                    .await;
                let ended = tokio::time::timeout(WAIT, call).await;
                tokio::time::resume();
                ensure!(matches!(
                    ended,
                    Ok(Ok(Err(ErrorCode::AuthorityUnavailable)))
                ));
            }
        }
        ensure!(!reader.stop.is_cancelled());
        let path = projection.join("audit.sqlite");
        ensure!(crate::tenancy::ownership::acquire_path(&path, false).is_err());
        if fatal {
            let close = reader.hooks.hold(Step::Close)?;
            let _release_close = close.release.clone().drop_guard();
            rollback.release.cancel();
            // No shutdown call or cancellation token is used to manufacture
            // this fence: only the read's preserved fatal classification can
            // take the owner here. The original implementation times out here.
            tokio::time::timeout(WAIT, close.entered.notified()).await?;
            ensure!(reader.stop.is_cancelled() && reader.sender.is_closed());
            ensure!(crate::tenancy::ownership::acquire_path(&path, false).is_err());
            let never_read = reader.hooks.hold(Step::Read)?;
            let _release_never_read = never_read.release.clone().drop_guard();
            let later = reader
                .read(
                    caller.clone(),
                    tenant.clone(),
                    read::Request::Page {
                        correlation: receipt.correlation_id.clone(),
                        after: None,
                        limit: 1,
                    },
                )
                .await;
            ensure!(
                matches!(later, Err(failure) if failure.0.code == ErrorCode::AuthorityUnavailable)
            );
            ensure!(never_read.entered.notified().now_or_never().is_none());
            let mut shutdown = Box::pin(reader.shutdown());
            ensure!(futures::poll!(&mut shutdown).is_pending());
            ensure!(crate::tenancy::ownership::acquire_path(&path, false).is_err());
            close.release.cancel();
            tokio::time::timeout(WAIT, shutdown).await??;
        } else {
            rollback.release.cancel();
            tokio::time::timeout(WAIT, async {
                while reader.reads.available_permits() != READS {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            ensure!(!reader.stop.is_cancelled() && !reader.sender.is_closed());
            let reply = reader
                .read(caller, tenant, read::Request::One(id))
                .await
                .map_err(|failure| anyhow::anyhow!("control read: {:?}", failure.0.code))?;
            ensure!(
                serde_json::from_slice::<serde_json::Value>(&reply.bytes)?
                    == serde_json::to_value(receipt)?
            );
            drop(reply);
            ensure!(!reader.stop.is_cancelled());
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    rollback.release.cancel();
    // Always settle fixture cleanup after collecting the independent fence
    // assertions, including red-baseline failures. Never abort the SQL owner.
    let cleanup = tokio::time::timeout(WAIT, reader.shutdown()).await;
    checked?;
    cleanup??;
    drop(crate::tenancy::ownership::acquire_path(
        &projection.join("audit.sqlite"),
        false,
    )?);
    ensure!(inventory(bundle)? == before);
    Ok(())
}
