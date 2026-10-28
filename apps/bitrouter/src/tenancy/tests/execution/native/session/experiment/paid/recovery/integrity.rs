//! Corrupt only genuine stopped native v4 bytes; bootstrap must not adopt them.
use super::*;
use sea_orm::{DatabaseConnection, TransactionTrait};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt};
mod corrupt;
mod repair;
mod state;

#[tokio::test]
#[ignore = "internal genuine v4 integrity opener; select a bounded parent"]
async fn opener() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("native integrity root")?);
    let mode = std::env::var(MODE)?;
    let (repeat, name) = match mode.strip_prefix("repeat:") {
        Some(name) => (true, name),
        None => (false, mode.as_str()),
    };
    let change = corrupt::Change::parse(name)?;
    let database = crate::db::connect_owned_tenancy(&format!(
        "sqlite://{}",
        root.join("native-paid.db").display()
    ))
    .await?;
    let db = database.database().clone();
    let checked = async {
        let expected: state::Snapshot = if repeat {
            state::read_json(&root.join("negative.json"))?
        } else {
            let before = state::snapshot(&db).await?;
            state::write_json(&root.join("original.json"), &before)?;
            let accepted: Value = state::read_json(&root.join("accepted.json"))?;
            let tx = db.begin().await?;
            let mutated = corrupt::apply(&tx, change, &accepted).await;
            match mutated {
                Ok(()) => tx.commit().await?,
                Err(error) => {
                    tx.rollback().await?;
                    return Err(error);
                }
            }
            let after = state::snapshot(&db).await?;
            before.check_negative_delta(&after)?;
            state::write_json(&root.join("negative.json"), &after)?;
            after
        };
        ensure!(state::snapshot(&db).await? == expected);
        let opened = Service::open_owned_profile(
            database,
            &crate::config::tenancy::TenancyConfig {
                enabled: true,
                wrapping_key_env: Some(KEY.into()),
                ..Default::default()
            },
            &crate::config::ContinuationConfig::default(),
        )
        .await;
        if let Ok(service) = opened {
            let stopped = service.shutdown().await;
            stopped?;
            anyhow::bail!("damaged native v4 {name} was adopted");
        }
        // This compares every scalar and application schema definition, including existing
        // receipts, Eval/trajectory closure, counters, Human heads and sources.
        // Generic startup failure alone is never proof of a read-only refusal.
        ensure!(
            state::snapshot(&db).await? == expected,
            "refused native v4 {name} changed stored facts"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = db.close().await;
    checked.and(closed.map_err(Into::into))?;
    // Prove the rejected startup released its actual owner before this child
    // exits; process death must not disguise a quarantined bootstrap as clean.
    drop(crate::tenancy::ownership::acquire_path(
        &root.join("native-paid.db"),
        false,
    )?);
    signal("native-paid-integrity-refused")?;
    Ok(())
}

async fn campaign(root: &Path, mode: &str) -> Result<()> {
    // Reserve the rest of the unchanged 120-second parent watchdog for the
    // existing Child's actual kill/wait and bounded diagnostic/listener drain.
    // An exhausted work window cannot start another child or become success.
    let work_deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    let subject = upstream().await?;
    let mut judge = Judge::start().await?;
    let subject_url = format!("{}/v1", subject.uri());
    let mut producer = match Child::start(
        "producer",
        root,
        "observed",
        Some((&subject_url, &judge.url)),
    ) {
        Ok(child) => child,
        Err(error) => return Err::<(), _>(error).and(judge.finish().await),
    };
    let produced = tokio::time::timeout_at(work_deadline, async {
        producer.line("native-paid-source-ready").await?;
        judge.retained_answer(state::read_json(&root.join("answer.json"))?)?;
        producer.advance().await?;
        producer.line("native-paid-accepted").await?;
        ensure!(judge.received().await?["model"] == "judge-model");
        judge.release.cancel();
        producer.line("native-paid-stopped").await?;
        let denied = crate::tenancy::ownership::acquire_path(&root.join("native-paid.db"), false)
            .err()
            .context("stopped producer released its original file owner")?;
        ensure!(matches!(
            denied.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ));
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|value| value);
    judge.release.cancel();
    let reaped = producer.finish(true, produced).await;
    let checked = async {
        reaped?;
        // Acquire the exact stopped file only AFTER actual old-process wait.
        // Preserve its raw DB/WAL plus original producer evidence; never restore
        // those bytes into a positive database or synthesize a replacement proof.
        let lease = crate::tenancy::ownership::acquire_path(&root.join("native-paid.db"), false)?;
        let archive = state::archive(root)?;
        drop(lease);
        for selector in [mode.to_owned(), format!("repeat:{mode}")] {
            ensure!(
                tokio::time::Instant::now() < work_deadline,
                "native integrity work deadline before child"
            );
            let mut child = Child::start("integrity::opener", root, &selector, None)?;
            let observed =
                tokio::time::timeout_at(work_deadline, child.line("native-paid-integrity-refused"))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|value| value);
            child.finish(false, observed).await?;
            drop(crate::tenancy::ownership::acquire_path(
                &root.join("native-paid.db"),
                false,
            )?);
            ensure!(
                state::archive_digest(root)? == archive,
                "original source archive changed"
            );
            ensure!(
                !judge.extra_request()
                    && subject
                        .received_requests()
                        .await
                        .context("subject requests")?
                        .len()
                        == 1,
                "refused bootstrap resent provider work"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    checked.and(judge.finish().await)
}

async fn parent(mode: &str) -> Result<()> {
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        Box::pin(campaign(root.path(), mode)),
    )
    .await;
    let result = result
        .context("finite native integrity parent deadline")
        .and_then(|value| value);
    if let Err(error) = result {
        return Err(error.context(format!(
            "native integrity artifacts retained at {}",
            root.keep().display()
        )));
    }
    Ok(())
}

macro_rules! negative {
    ($test:ident, $mode:literal) => {
        #[tokio::test]
        async fn $test() -> Result<()> {
            parent($mode).await
        }
    };
}
negative!(
    actual_v4_foreign_tenant_projection_refuses_twice_without_repair,
    "tenant"
);
negative!(
    actual_v4_foreign_creator_projection_refuses_twice_without_repair,
    "principal"
);
negative!(
    actual_v4_changed_native_evidence_refuses_twice_without_repair,
    "evidence"
);
negative!(
    actual_v4_changed_known_receipt_digest_refuses_twice_without_repair,
    "receipt"
);
negative!(
    actual_v4_changed_pending_mirror_refuses_twice_without_repair,
    "mirror"
);
negative!(
    actual_v4_missing_native_close_ack_refuses_twice_without_repair,
    "close-ack"
);
negative!(
    actual_v4_wrong_cleanup_epoch_refuses_twice_without_repair,
    "epoch"
);
negative!(
    actual_v4_missing_registry_stop_set_refuses_twice_without_repair,
    "stop-set"
);
negative!(
    actual_v4_extra_stop_member_with_valid_digest_refuses_twice_without_repair,
    "extra-member"
);

#[tokio::test]
async fn actual_v4_integrity_clean_bootstrap_control_retains_original_known_receipt() -> Result<()>
{
    // Same real producer, connector and two fresh-process bootstraps as the
    // negatives, using the existing fully checked positive recovery consumer.
    recovery_parent("observed").await
}
