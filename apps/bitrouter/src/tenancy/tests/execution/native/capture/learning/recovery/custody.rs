//! The real bootstrap owner survives consuming SQL errors and a caught unwind.
use super::*;
use crate::tenancy::transaction::{Disposition, Phase};

#[tokio::test]
#[ignore = "isolated real paid bootstrap SQL/H custody; select this parent"]
async fn paid_bootstrap_actual_begin_error_keeps_owner_until_process_exit() -> Result<()> {
    restart::campaign("begin").await
}
#[tokio::test]
#[ignore = "isolated real paid bootstrap SQL/H custody; select this parent"]
async fn paid_bootstrap_actual_consuming_rollback_error_keeps_owner_until_process_exit()
-> Result<()> {
    restart::campaign("rollback").await
}
#[tokio::test]
#[ignore = "isolated real paid bootstrap SQL/H custody; select this parent"]
async fn paid_bootstrap_caught_unwind_keeps_owner_until_process_exit() -> Result<()> {
    restart::campaign("unwind").await
}
#[tokio::test]
#[ignore = "isolated real paid bootstrap cancelled waiter; select this parent"]
async fn paid_cancelled_bootstrap_waiter_does_not_cancel_the_claimed_writer() -> Result<()> {
    restart::campaign("cancel").await
}

#[tokio::test]
#[ignore = "internal paid bootstrap fault owner; select its parent only"]
async fn opener() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("paid root")?);
    let mode = std::env::var(MODE)?;
    let database = crate::db::connect_owned_tenancy(&url(&root)).await?;
    let db = database.database().clone();
    if mode == "rollback" {
        // The real DML ends SQLite's transaction. The real consuming SeaORM
        // rollback must then return an error; no synthetic disposition is set.
        db.execute_unprepared("CREATE TRIGGER paid_repair_rollback BEFORE INSERT ON acp_assessment_revisions BEGIN SELECT RAISE(ROLLBACK, 'paid repair rollback fixture'); END").await?;
    }
    let before = exact(&db).await?;
    let (attached, receive) = tokio::sync::oneshot::channel();
    let mut opening = AbortOnDropHandle::new(tokio::spawn(async move {
        Service::open_owned_profile_observed_startup(
            database,
            &config(),
            &crate::config::ContinuationConfig::default(),
            crate::telemetry::receipt_latency::Sink::default(),
            Some(crate::tenancy::StartupTest {
                bootstrap: Some(crate::tenancy::BootstrapGate {
                    phase: Phase::RollingBack,
                    attached,
                }),
                #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
                bind: None,
            }),
        )
        .await
    }));
    let (proof, epoch) = tokio::time::timeout(TIMEOUT, receive).await??;
    tokio::time::timeout(TIMEOUT, epoch.entered.notified()).await?;
    // The existing031 pin validation is read-only. Its first rollback precedes
    // the paid prepare BEGIN; replace the one test gate while it is held.
    let phase = if mode == "unwind" {
        Phase::Active
    } else {
        Phase::Beginning
    };
    let paid = proof.hold(phase);
    epoch.release.notify_one();
    tokio::time::timeout(TIMEOUT, paid.entered.notified()).await?;
    ensure!(proof.disposition() == Disposition::Pending);
    locked(&root)?;
    match mode.as_str() {
        "begin" => {
            db.execute_unprepared("BEGIN").await?;
            proof.release_gate();
        }
        "rollback" => proof.release_gate(),
        "unwind" => proof.unwind_gate()?,
        "cancel" => {
            opening.abort();
            ensure!(matches!((&mut opening).await,Err(error) if error.is_cancelled()));
            drop(db);
            locked(&root)?;
            // The worker is still at a real BEGIN boundary and owns the claim.
            signal("paid-bootstrap-owner-retained")?;
            return blocked().await;
        }
        _ => anyhow::bail!("unknown paid bootstrap fault"),
    }
    let opened = tokio::time::timeout(TIMEOUT, opening).await??;
    if let Ok(service) = opened {
        service.shutdown().await?;
        anyhow::bail!("faulted paid bootstrap published a Service");
    }
    tokio::time::timeout(TIMEOUT, proof.quarantined()).await?;
    ensure!(matches!(
        proof.disposition(),
        Disposition::Pending | Disposition::Ambiguous
    ));
    drop(db); // no test-side writer pool may account for the retained lease
    locked(&root)?;
    let reader = crate::db::connect(&format!("{}?mode=ro", url(&root))).await?;
    let unchanged = exact(&reader).await;
    let closed = reader.close().await;
    ensure!(
        unchanged? == before,
        "unknown bootstrap changed immutable paid/receipt/head rows"
    );
    closed?;
    signal("paid-bootstrap-owner-retained")?;
    blocked().await
}
