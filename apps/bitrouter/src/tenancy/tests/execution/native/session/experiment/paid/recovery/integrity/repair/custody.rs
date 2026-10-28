//! Fault the same paid bootstrap apply owner, after its read proposal is fixed.
use super::*;

#[tokio::test]
#[ignore = "internal native paid BEGIN/ROLLBACK/unwind owner; select a bounded parent"]
async fn opener() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("native repair root")?);
    let mode = std::env::var(MODE)?;
    ensure!(matches!(
        mode.as_str(),
        "begin" | "rollback" | "unwind" | "cancel-begin"
    ));
    let (database, db, original, _) = begin_fixture(&root).await?;
    if mode == "rollback" {
        // Missing035 repair actually inserts this original paid terminal. The
        // trigger ends SQLite's transaction; H's consuming rollback must fail.
        db.execute_unprepared("CREATE TRIGGER native_paid_repair_rollback BEFORE INSERT ON tenancy_execution_receipts \
            WHEN EXISTS (SELECT 1 FROM checkpoint_evolution WHERE kind='owned_judge_active_v3' \
            AND json_extract(body,'$.version')=4 AND json_extract(body,'$.execution')=NEW.execution_id) \
            BEGIN SELECT RAISE(ROLLBACK, 'native paid repair rollback fixture'); END").await?;
    }
    let before = state::snapshot(&db).await?;
    let (attached, receive) = tokio::sync::oneshot::channel();
    let mut opening = Some(AbortOnDropHandle::new(tokio::spawn(async move {
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
    })));
    let (proof, prepared) = tokio::time::timeout(TIMEOUT, receive).await??;
    let checked = async {
        // Successful continuation initialization commits. The first rollback
        // belongs to paid prepare: its proposal is already fixed outside H.
        tokio::time::timeout(TIMEOUT, prepared.entered.notified()).await?;
        let phase = if matches!(mode.as_str(), "begin" | "cancel-begin") {
            Phase::Beginning
        } else {
            Phase::Active
        };
        let apply = proof.hold(phase);
        prepared.release.notify_one();
        tokio::time::timeout(TIMEOUT, apply.entered.notified()).await?;
        ensure!(proof.disposition() == Disposition::Pending);
        locked(&root)?;
        let counts = proof.sql_counts_for_test();
        match mode.as_str() {
            "begin" => {
                // The single physical connection is free at this gate. SQLite
                // accepts this real BEGIN; the actor's original BEGIN then fails.
                db.execute_unprepared("BEGIN").await?;
                proof.release_gate();
            }
            "rollback" => {
                let rollback = proof.hold(Phase::RollingBack);
                apply.release.notify_one();
                tokio::time::timeout(TIMEOUT, rollback.entered.notified()).await?;
                ensure!(
                    proof.disposition() == Disposition::Pending
                        && proof.sql_counts_for_test() == counts
                );
                proof.release_gate();
            }
            "unwind" => proof.unwind_gate()?,
            "cancel-begin" => {
                let mut waiter = opening.take().context("opening waiter")?;
                waiter.abort();
                ensure!(matches!((&mut waiter).await,Err(error) if error.is_cancelled()));
            }
            _ => anyhow::bail!("unknown custody mode"),
        }
        drop(db); // Proof/gates own no pool; only the original H frame may hold the claim.
        if mode != "cancel-begin" {
            let result =
                tokio::time::timeout(TIMEOUT, opening.take().context("opening result")?).await??;
            if let Ok(service) = result {
                let cleanup = service.shutdown().await;
                anyhow::bail!("faulted native bootstrap published authority: {cleanup:?}");
            }
            tokio::time::timeout(TIMEOUT, proof.quarantined()).await?;
            ensure!(matches!(
                proof.disposition(),
                Disposition::Pending | Disposition::Ambiguous
            ));
            let after = proof.sql_counts_for_test();
            ensure!(
                after.1 == counts.1,
                "consuming failure/unwind gained a false rollback ACK"
            );
            if mode == "begin" {
                ensure!(after.0 == counts.0 + 1);
            } else {
                ensure!(after.0 == counts.0);
            }
        } else {
            ensure!(
                proof.disposition() == Disposition::Pending
                    && proof.sql_counts_for_test() == counts
            );
        }
        let reader = crate::db::connect(&format!("{}?mode=ro", url(&root))).await?;
        let same = async {
            ensure!(
                state::snapshot(&reader).await? == before,
                "failed native repair changed original durable facts"
            );
            absent(&reader, key(&original)?.execution.as_str()).await
        }
        .await;
        let closed = reader.close().await;
        same.and(closed.map_err(Into::into))?;
        locked(&root)?;
        signal("native-paid-repair-unknown-retained")?;
        blocked().await
    }
    .await;
    prepared.release.notify_one();
    proof.release_gate();
    checked
}
