//! Real process death only after actual host cleanup and the old Registry stop-set.
use super::*;
use crate::tenancy::transaction::{Disposition, receipt_observation::Key};
use base64::Engine;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
mod integrity;
mod process;
mod reduced;
mod unknown;
use process::Child;
const ROOT: &str = "BITROUTER_NATIVE_PAID_RECOVERY_ROOT";
const MODE: &str = "BITROUTER_NATIVE_PAID_RECOVERY_MODE";
const KEY: &str = "BITROUTER_NATIVE_PAID_RECOVERY_KEY";
const SUBJECT: &str = "BITROUTER_NATIVE_PAID_RECOVERY_SUBJECT";
const JUDGE: &str = "BITROUTER_NATIVE_PAID_RECOVERY_JUDGE";
const PREFIX: &str = "tenancy::tests::execution::native::session::experiment::paid::recovery::";

fn signal(value: &str) -> Result<()> {
    println!("\n{value}");
    std::io::stdout().flush()?;
    Ok(())
}
async fn advance() -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin().take(64));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(45), input.read_line(&mut line)).await??;
    ensure!(
        line.trim() == "release-commit",
        "parent scheduling acknowledgement changed"
    );
    Ok(())
}
async fn rows(db: &sea_orm::DatabaseConnection, kind: &str) -> Result<Vec<Value>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT body FROM checkpoint_evolution WHERE kind=? ORDER BY record_id LIMIT 129",
            [kind.into()],
        ))
        .await?;
    ensure!(rows.len() <= 128);
    rows.into_iter()
        .map(|r| {
            let body: String = r.try_get("", "body")?;
            ensure!(body.len() <= 1024 * 1024);
            Ok(serde_json::from_str(&body)?)
        })
        .collect()
}
async fn accepted(db: &sea_orm::DatabaseConnection) -> Result<Value> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let jobs = rows(db, "owned_judge_active_v3").await?;
            ensure!(jobs.len() <= 1);
            if let Some(job) = jobs.into_iter().next() {
                return Ok::<_, anyhow::Error>(job);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}
async fn absent(db: &sea_orm::DatabaseConnection, execution: &str) -> Result<()> {
    ensure!(
        db.query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM tenancy_execution_receipts WHERE execution_id=?",
            [execution.into()]
        ))
        .await?
        .is_none(),
        "actual paid terminal unexpectedly durable"
    );
    Ok(())
}
async fn clean_rollback(fixture: &Fixture, key: &Key) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let trace = fixture.service.cleanup.receipt_trace_for_test()?;
            ensure!(trace.key == *key && !trace.overflow);
            if let Some(fact) = trace
                .facts
                .iter()
                .flatten()
                .find(|fact| fact.rollback.is_some())
            {
                ensure!(
                    fact.begin.is_some_and(|v| v.ok)
                        && fact
                            .store
                            .is_some_and(|v| !v.ok && v.sqlite_code == Some(1811))
                        && fact.rollback.is_some_and(|v| v.ok)
                        && fact.commit.is_none()
                        && !fact.check_failed
                        && fact.disposition == Disposition::RolledBack,
                    "missing terminal must come from actual confirmed SQLite INSERT rollback"
                );
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    fixture.service.cleanup.ensure_clean()?;
    absent(&fixture.db, key.execution.as_str()).await
}

#[tokio::test]
#[ignore = "internal native paid producer; select bounded process parent"]
async fn producer() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("native paid root")?);
    let mode = std::env::var(MODE)?;
    let subject = std::env::var(SUBJECT)?;
    let (fixture, headers) = ready(&root, &subject, &std::env::var(JUDGE)?).await?;
    let gate = Arc::new(crate::tenancy::receipts::authority_test::CredentialGate::default());
    let mut session = None;
    let mut shutdown = None;
    let checked: Result<()> = async {
        session = Some(open(&fixture, &headers, &subject, &root, "native-paid-stopped").await?);
        let current = session.as_mut().context("actual native owner")?;
        current.run(human::observed_task(), Arc::new(Approval), CancellationToken::new()).await?;
        let review = current.review_run().await?;
        let answer = human::label(&review, "actual-child-source", true)?.evaluation;
        let body = serde_json::to_vec(&answer)?; ensure!(body.len() <= 64 * 1024);
        std::fs::write(root.join("answer.json"), body)?;
        if mode == "missing" {
            fixture.db.execute_unprepared("CREATE TRIGGER native_paid_terminal_abort BEFORE INSERT ON tenancy_execution_receipts \
                WHEN EXISTS (SELECT 1 FROM checkpoint_evolution WHERE kind='owned_judge_active_v3' \
                AND json_extract(body,'$.execution')=NEW.execution_id) \
                BEGIN SELECT RAISE(ABORT, 'native paid terminal rollback fixture'); END").await?;
        } else {
            *fixture.service.paid_finish_gate.lock().map_err(|_| anyhow::anyhow!("paid finish gate"))? = Some(gate.clone());
        }
        signal("native-paid-source-ready")?; advance().await?;
        let mut paid = Box::pin(current.assess_run(&review.scoring.evidence.checkpoint_id, None));
        let job = tokio::select! {
            reply = &mut paid => { reply?; anyhow::bail!("paid caller returned before actual acceptance"); }
            job = accepted(&fixture.db) => job?,
        };
        let seed = &job["recovery"]["seed"];
        let key = Key { installation: seed["installation"].as_str().context("installation")?.try_into()?,
            tenant: seed["tenant"].as_str().context("tenant")?.try_into()?,
            execution: seed["execution"].as_str().context("execution")?.try_into()? };
        if mode == "missing" { fixture.service.cleanup.observe_receipt_for_test(key.clone())?; }
        signal("native-paid-accepted")?;
        if mode == "missing" { clean_rollback(&fixture, &key).await?; }
        else { tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?; }
        // This drops only a public result waiter; the existing host still polls
        // its real provider/observation work and owns local cleanup.
        drop(paid);
        let current = session.take().context("actual closing owner")?;
        tokio::time::timeout(TIMEOUT, current.close()).await??;
        ensure!(fixture.service.current().is_ok());
        fixture.service.hosts.close();
        tokio::time::timeout(TIMEOUT, fixture.service.hosts.drain()).await??;
        let service = fixture.service.clone();
        shutdown = Some(AbortOnDropHandle::new(tokio::spawn(async move { service.shutdown().await })));
        let stopped = tokio::time::timeout(TIMEOUT, async {
            loop {
                let job = accepted(&fixture.db).await?;
                if job["recovery"]["stopped"].is_object() && job["recovery"]["native_cleanup"].is_object() {
                    return Ok::<_, anyhow::Error>(job);
                }
                tokio::task::yield_now().await;
            }
        }).await??;
        ensure!(stopped["recovery"]["seed"]["native"]["source"]["checkpoint"] == review.scoring.evidence.checkpoint_id);
        if mode == "missing" {
            absent(&fixture.db, key.execution.as_str()).await?;
            ensure!(stopped["output"].is_null() && stopped["recovery"]["observed"].is_null());
            if shutdown.as_ref().is_some_and(|task| task.is_finished()) {
                ensure!(shutdown.take().context("shutdown task")?.await?.is_err());
            }
        } else {
            ensure!(stopped["output"]["usage"]["prompt_tokens"] == 4 && stopped["recovery"]["observed"].is_object());
            ensure!(shutdown.as_ref().is_some_and(|task| !task.is_finished()));
        }
        let body = serde_json::to_vec(&stopped)?; ensure!(body.len() <= 1024 * 1024);
        std::fs::write(root.join("accepted.json"), body)?;
        let denied = crate::tenancy::ownership::acquire_path(&root.join("native-paid.db"), false).err().context("owner released before process death")?;
        ensure!(matches!(denied.downcast_ref::<std::fs::TryLockError>(), Some(std::fs::TryLockError::WouldBlock)));
        signal("native-paid-stopped")?;
        advance().await?;
        anyhow::bail!("bounded parent must reap this stopped old owner")
    }.await;
    gate.release.cancel();
    let closed = match session.take() {
        Some(session) => tokio::time::timeout(TIMEOUT, session.close())
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r.map_err(Into::into)),
        None => Ok(()),
    };
    let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    drop(shutdown);
    checked
        .and(closed)
        .and(stopped.map_err(anyhow::Error::from).and_then(|r| r))
}

#[tokio::test]
#[ignore = "internal real production bootstrap; select bounded process parent"]
async fn consumer() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("native paid root")?);
    let mode = std::env::var(MODE)?;
    let before = std::fs::read(root.join("accepted.json"))?;
    ensure!(before.len() <= 1024 * 1024);
    let before: Value = serde_json::from_slice(&before)?;
    let database = crate::db::connect_owned_tenancy(&format!(
        "sqlite://{}",
        root.join("native-paid.db").display()
    ))
    .await?;
    let db = database.database().clone();
    if mode == "missing" {
        // The parent has killed and reaped the actual old owner before opening
        // this exclusively leased pool. Remove only this campaign's INSERT
        // fault, never a receipt, accepted record, seed or cleanup proof. The
        // second real restart also reaches this idempotent test-only cleanup.
        db.execute_unprepared("DROP TRIGGER IF EXISTS native_paid_terminal_abort")
            .await?;
    }
    let service = Service::open_owned_profile(
        database,
        &crate::config::tenancy::TenancyConfig {
            enabled: true,
            wrapping_key_env: Some(KEY.into()),
            ..Default::default()
        },
        &crate::config::ContinuationConfig::default(),
    )
    .await?;
    let checked = async {
        let jobs = rows(&db, "owned_judge_job").await?;
        ensure!(
            jobs.len() == 1
                && jobs[0]["job"] == before["job"]
                && jobs[0]["execution"] == before["execution"]
        );
        ensure!(
            jobs[0]["recovery"]["seed"] == before["recovery"]["seed"]
                && jobs[0]["native_evidence"] == before["native_evidence"]
        );
        ensure!(
            rows(&db, "owned_judge_pending").await?.is_empty()
                && rows(&db, "owned_judge_active_v3").await?.is_empty()
        );
        let counters = rows(&db, "owned_judge_budget").await?;
        ensure!(counters.len() == 1 && counters[0]["accepted"] == 1);
        let seed = &before["recovery"]["seed"];
        let receipt = crate::tenancy::receipts::store::lookup(
            &db,
            &seed["installation"]
                .as_str()
                .context("installation")?
                .try_into()?,
            &seed["tenant"].as_str().context("tenant")?.try_into()?,
            &seed["execution"]
                .as_str()
                .context("execution")?
                .try_into()?,
        )
        .await?
        .context("repaired actual035")?;
        if mode == "missing" {
            ensure!(
                jobs[0]["status"] == "interrupted"
                    && jobs[0]["output"].is_null()
                    && jobs[0]["assessment"].is_null()
            );
            ensure!(
                receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Unknown
                    && receipt.outcome == bitrouter_ai::tenancy::execution::Outcome::Unknown
                    && receipt.attempts.is_empty()
                    && receipt.usage.input_tokens.is_none()
                    && receipt.usage.output_tokens.is_none()
                    && !receipt.usage.complete
            );
        } else {
            ensure!(jobs[0]["status"] == "unselected" && jobs[0]["output"] == before["output"]);
            ensure!(
                receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed
                    && receipt.usage.input_tokens == Some(4)
                    && receipt.usage.output_tokens == Some(2)
            );
        }
        ensure!(
            service.current().is_ok()
                && service
                    .admission
                    .receipt_capacity_for_test()
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?
                    .0
                    == 0
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = service.shutdown().await;
    let closed = db.close().await;
    checked.and(stopped).and(closed.map_err(Into::into))?;
    signal("native-paid-recovered")?;
    Ok(())
}

async fn recovery_parent(mode: &str) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(120),
        Box::pin(async {
            let subject = upstream().await?;
            let mut judge = Judge::start().await?;
            let root = tempfile::tempdir()?;
            let subject_url = format!("{}/v1", subject.uri());
            let mut producer = Child::start(
                "producer",
                root.path(),
                mode,
                Some((&subject_url, &judge.url)),
            )?;
            let witnessed = async {
                producer.line("native-paid-source-ready").await?;
                let answer = std::fs::read(root.path().join("answer.json"))?;
                ensure!(answer.len() <= 64 * 1024);
                judge.retained_answer(serde_json::from_slice(&answer)?)?;
                producer.advance().await?;
                producer.line("native-paid-accepted").await?;
                ensure!(judge.received().await?["model"] == "judge-model");
                judge.release.cancel();
                producer.line("native-paid-stopped").await
            }
            .await;
            judge.release.cancel();
            let reaped = producer.finish(true, witnessed).await;
            let result = async {
                reaped?;
                for _ in 0..2 {
                    let mut consumer = Child::start("consumer", root.path(), mode, None)?;
                    let observed = consumer.line("native-paid-recovered").await;
                    consumer.finish(false, observed).await?;
                }
                ensure!(
                    !judge.extra_request()
                        && subject
                            .received_requests()
                            .await
                            .context("subject sends")?
                            .len()
                            == 1
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            result.and(judge.finish().await)
        }),
    )
    .await?
}
#[tokio::test]
async fn actual_native_closed_stop_set_recovers_observed_paid_output_without_head_selection_or_resend()
-> Result<()> {
    recovery_parent("observed").await
}
#[tokio::test]
async fn actual_native_missing035_rollback_close_stop_set_recovers_unknown_without_refund_or_resend()
-> Result<()> {
    recovery_parent("missing").await
}
