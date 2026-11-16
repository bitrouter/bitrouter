//! Real process death only after actual host cleanup and the old Registry stop-set.
use super::*;
use base64::Engine;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
mod integrity;
pub(super) mod process;
mod reduced;
pub(super) mod terminal;
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
pub(super) async fn rows(db: &sea_orm::DatabaseConnection, kind: &str) -> Result<Vec<Value>> {
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
