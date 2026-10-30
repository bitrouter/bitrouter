//! Real checked capture/actor -> native judge -> receipt -> assessment consumer.
//! Most cases use finite recorded ACP observations; the ignored controller case
//! also crosses the separately built Rust child and public checked facade.
#![cfg(feature = "core")]
use super::*;
use crate::tenancy::actor::OperationLimits;
use crate::{
    harness::acp::capture::CapturePort,
    storage::capture::{CaptureDirection as Direction, CaptureEvent, CaptureKind as Kind},
    tenancy::capture::port::Port,
};
use chrono::Utc;
#[cfg(unix)]
mod coexistence;
mod custody;
mod faults;
mod fixture;
mod human;
mod qualification;
#[cfg(unix)]
mod recovery;
mod startup;
use fixture::{Judge, Ready, bodies, drained, native};

async fn cleanup(
    ready: &Ready,
    ports: &[Arc<Port>],
    judge: Judge,
    provider: Provider,
    result: Result<()>,
) -> Result<()> {
    judge.release.cancel();
    let mut closed = Ok(());
    for port in ports {
        closed = closed.and(port.finish().await);
    }
    let shutdown = tokio::time::timeout(TIMEOUT, ready.fixture.service.shutdown()).await;
    let judge_stop = judge.stop().await;
    let provider_stop = provider.finish().await;
    result
        .and(closed)
        .and(
            shutdown
                .context("learning shutdown deadline")
                .and_then(|value| value),
        )
        .and(judge_stop)
        .and(provider_stop)
}

async fn receipt(ready: &Ready, execution: &str) -> Result<Value> {
    let row = ready
        .fixture
        .db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT receipt_json FROM tenancy_execution_receipts WHERE execution_id=?",
            [execution.into()],
        ))
        .await?
        .context("actual judge receipt missing")?;
    Ok(serde_json::from_str(
        &row.try_get::<String>("", "receipt_json")?,
    )?)
}

#[tokio::test]
async fn explicit_observe_judge_uses_owned_native_gate_and_duplicate_job_charges_once() -> Result<()>
{
    let provider = Provider::start(false).await?;
    let mut judge = Judge::new(false).await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let port = ready.port().await?;
    let checked = async {
        let summary = port.checkpoint(native()).await?.context("checkpoint")?;
        drained(&ready).await?;
        let observed = judge.receive().await?;
        ensure!(observed.body["model"] == "judge-model");
        ensure!(
            observed
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some("Bearer explicit-judge-secret")
        );
        let wire = serde_json::to_string(&observed.body)?;
        ensure!(
            wire.contains("explicit recorded task")
                && !wire.contains(native())
                && !wire.contains("sessionId")
                && !wire.contains("owned-upstream-secret")
                && !wire.contains("explicit-judge-secret")
        );
        ensure!(observed.body["tools"].as_array().is_none_or(Vec::is_empty));
        let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
        ensure!(jobs.len() == 1 && jobs[0]["status"] == "completed");
        let execution = jobs[0]["execution"].as_str().context("execution link")?;
        let receipt = receipt(&ready, execution).await?;
        ensure!(
            receipt["state"] == "committed"
                && receipt["attempts"].as_array().context("attempts")?.len() == 1,
            "judge receipt state={} usage={}",
            receipt["state"],
            receipt["usage"]
        );
        ensure!(
            jobs[0]["output"]["request_id"] == execution
                && jobs[0]["output"]["usage"]["prompt_tokens"] == 4
        );
        ensure!(count(&ready.fixture, "acp_assessment_revisions").await? == 1);
        ensure!(
            bodies(&ready.fixture, "owned_judge_pending")
                .await?
                .is_empty()
        );
        ensure!(port.checkpoint(native()).await? == Some(summary));
        drained(&ready).await?;
        ensure!(judge.calls.load(Ordering::SeqCst) == 1 && provider.calls() == 0);
        let counters = bodies(&ready.fixture, "owned_judge_budget").await?;
        ensure!(counters.len() == 1 && counters[0]["accepted"] == 1);
        ensure!(bodies(&ready.fixture, "judge_job").await?.is_empty());
        Ok(())
    }
    .await;
    cleanup(&ready, &[port], judge, provider, checked).await
}

#[tokio::test]
async fn completed_judge_with_partial_provider_usage_preserves_incomplete_receipt() -> Result<()> {
    let provider = Provider::start(false).await?;
    let judge = Judge::incomplete_usage().await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let port = ready.port().await?;
    let checked = async {
        port.checkpoint(native()).await?.context("checkpoint")?;
        drained(&ready).await?;
        let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
        ensure!(jobs.len() == 1 && jobs[0]["status"] == "completed");
        let execution = jobs[0]["execution"].as_str().context("execution link")?;
        let receipt = receipt(&ready, execution).await?;
        ensure!(receipt["state"] == "incomplete" && receipt["outcome"] == "completed");
        ensure!(receipt["attempts"].as_array().context("attempts")?.len() == 1);
        let usage = &receipt["usage"];
        ensure!(usage["input_tokens"] == 4 && usage["output_tokens"] == 2);
        ensure!(usage["provenance"] == "provider" && usage["complete"] == false);
        for field in [
            "cache_read_tokens",
            "cache_write_tokens",
            "reasoning_tokens",
        ] {
            ensure!(usage.get(field).is_some_and(Value::is_null));
        }
        ensure!(count(&ready.fixture, "acp_assessment_revisions").await? == 1);
        ensure!(judge.calls.load(Ordering::SeqCst) == 1 && provider.calls() == 0);
        Ok(())
    }
    .await;
    cleanup(&ready, &[port], judge, provider, checked).await
}

#[tokio::test]
async fn absent_profile_off_and_zero_budget_never_send_paid_work() -> Result<()> {
    for (profile, mode, maximum) in [
        (false, LearningMode::Observe, 1),
        (true, LearningMode::Off, 0),
        (true, LearningMode::Observe, 0),
    ] {
        let provider = Provider::start(false).await?;
        let judge = Judge::new(false).await?;
        let ready = Ready::new(&provider.url, &judge.url, profile, mode, maximum).await?;
        let port = ready.port().await?;
        let checked = async {
            let result = port.checkpoint(native()).await;
            if profile && mode != LearningMode::Off {
                ensure!(result.is_err());
            } else {
                result?;
            }
            drained(&ready).await?;
            ensure!(judge.calls.load(Ordering::SeqCst) == 0 && provider.calls() == 0);
            ensure!(bodies(&ready.fixture, "owned_judge_job").await?.is_empty());
            Ok(())
        }
        .await;
        cleanup(&ready, &[port], judge, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn final_revision_unit_is_atomic_and_restart_does_not_refund_it() -> Result<()> {
    let provider = Provider::start(false).await?;
    let judge = Judge::new(false).await?;
    let mut ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let first = ready.port().await?;
    let second = ready.port().await?;
    let mut ports = vec![first.clone(), second.clone()];
    let checked = async {
        let (one, two) = tokio::join!(first.checkpoint(native()), second.checkpoint(native()));
        ensure!(
            one.is_ok() != two.is_ok(),
            "exactly one final-unit job must be accepted"
        );
        drained(&ready).await?;
        ensure!(judge.calls.load(Ordering::SeqCst) == 1);
        for port in &ports {
            port.finish().await?;
        }
        ports.clear();
        ready.restart().await?;
        let after_restart = ready.port().await?;
        ports.push(after_restart.clone());
        ensure!(after_restart.checkpoint(native()).await.is_err());
        drained(&ready).await?;
        ensure!(judge.calls.load(Ordering::SeqCst) == 1);
        ready.revision = ready
            .configuration("assessment-r2", LearningMode::Observe, 1)
            .await?;
        let next_revision = ready.port().await?;
        ports.push(next_revision.clone());
        ensure!(next_revision.checkpoint(native()).await?.is_some());
        drained(&ready).await?;
        let budgets = bodies(&ready.fixture, "owned_judge_budget").await?;
        ensure!(budgets.len() == 2 && budgets.iter().all(|value| value["accepted"] == 1));
        ensure!(judge.calls.load(Ordering::SeqCst) == 2);
        Ok(())
    }
    .await;
    cleanup(&ready, &ports, judge, provider, checked).await
}

#[tokio::test]
async fn admitted_judge_keeps_usage_after_revoke_or_suspend_without_selecting_a_new_head()
-> Result<()> {
    for change in ["revoke", "suspend"] {
        let provider = Provider::start(false).await?;
        let mut judge = Judge::new(true).await?;
        let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 2).await?;
        let port = ready.port().await?;
        let owned = port.clone();
        let job = AbortOnDropHandle::new(tokio::spawn(
            async move { owned.checkpoint(native()).await },
        ));
        let checked = async {
            judge.receive().await?;
            change_authority(&ready.fixture, &ready.credential, &ready.subject, change).await?;
            judge.release.cancel();
            // An already sent result may complete. A rejected checkpoint reply
            // is not proof of zero model usage or a refundable acceptance.
            let _reply = tokio::time::timeout(TIMEOUT, job).await??;
            drained(&ready).await?;
            let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
            ensure!(jobs.len() == 1 && jobs[0]["status"] != "accepted");
            let receipt =
                receipt(&ready, jobs[0]["execution"].as_str().context("execution")?).await?;
            ensure!(receipt["attempts"].as_array().context("attempts")?.len() == 1);
            ensure!(
                receipt["usage"]["input_tokens"] == 4
                    && receipt["usage"]["output_tokens"] == 2
                    && receipt["usage"]["provenance"] == "provider"
            );
            ensure!(count(&ready.fixture, "acp_effective_assessments").await? == 0);
            ensure!(port.checkpoint(native()).await.is_err());
            ensure!(
                judge.calls.load(Ordering::SeqCst) == 1
                    && bodies(&ready.fixture, "owned_judge_budget").await?[0]["accepted"] == 1
            );
            Ok(())
        }
        .await;
        cleanup(&ready, &[port], judge, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn real_checked_controller_checkpoint_is_the_paid_assessment_initiator() -> Result<()> {
    let program = executable()?;
    let mut provider = Provider::start(false).await?;
    let mut judge = Judge::new(false).await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("learning-child.json");
    let spies = Arc::new(HostSpies::default());
    let app = Arc::new(
        host_app(&ready.fixture, &provider.url, spies.clone())?
            .with_tenancy(ready.fixture.service.clone()),
    );
    let listener = Listener::start(app.clone()).await?;
    let mut session = None;
    let checked = async {
        session = Some(
            ControlledSession::open_authenticated(
                app,
                ready.headers()?,
                launch(&program, &marker, &listener.origin, None),
                CancellationToken::new(),
            )
            .await?,
        );
        let current = session.as_ref().context("controller")?;
        let native = current.new_session(directory.path().to_owned()).await?;
        current
            .prompt(&native, "explicit task for owned assessment")
            .await?;
        provider.receive().await?;
        drained(&ready).await?;
        ensure!(
            judge.calls.load(Ordering::SeqCst) == 0,
            "prompt must not auto-discover a judge"
        );
        ensure!(current.checkpoint(&native).await?.is_some());
        judge.receive().await?;
        drained(&ready).await?;
        ensure!(judge.calls.load(Ordering::SeqCst) == 1 && provider.calls() == 1);
        ensure!(
            spies.decisions.load(Ordering::SeqCst) == 0
                && spies.transforms.load(Ordering::SeqCst) == 0
        );
        ensure!(count(&ready.fixture, "acp_assessment_revisions").await? == 1);
        Ok(())
    }
    .await;
    let closed = match session {
        Some(session) => session.shutdown().await,
        None => Ok(()),
    };
    let stopped = listener.finish().await;
    let reaped = reaped(&marker).await;
    cleanup(
        &ready,
        &[],
        judge,
        provider,
        checked.and(closed).and(stopped).and(reaped),
    )
    .await
}
