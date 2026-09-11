use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

use bitrouter_sdk::App;
use bitrouter_sdk::acp::capture::{CaptureDirection, CapturePort};
use bitrouter_sdk::config::{Config, ConfigRoutingTable};
use bitrouter_sdk::language_model::executor::MockExecutor;
use bitrouter_sdk::language_model::{HookDecision, PipelineContext, PreRequestHook, ToolChoice};
use serde_json::json;

use crate::acp_trajectory::checkpoint::types::AssessmentSource;
use crate::acp_trajectory::{Recorder, RecordingScope};
use crate::evolution::{evidence::EvidencePacket, scoring};

struct CountRequests(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl PreRequestHook for CountRequests {
    async fn check(&self, ctx: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(ctx.prompt().tools.is_empty());
        assert_eq!(ctx.prompt().tool_choice, Some(ToolChoice::None));
        assert!(ctx.prompt().params.max_tokens.is_none());
        assert!(!ctx.headers().contains_key("x-bitrouter-controller-id"));
        Ok(HookDecision::Allow)
    }
}

struct Fixture {
    runtime: EvolutionRuntime,
    canonical: CanonicalStore,
    recorder: Arc<Recorder>,
    identity: SessionIdentity,
    pipeline: Arc<Pipeline>,
    calls: Arc<AtomicUsize>,
    config: Config,
    _home: tempfile::TempDir,
}

async fn fixture(mode: EvolutionMode) -> Result<Fixture> {
    fixture_with_storage(mode, false).await
}

async fn fixture_with_storage(mode: EvolutionMode, persistent: bool) -> Result<Fixture> {
    let home = tempfile::tempdir()?;
    let database = if persistent {
        format!(
            "sqlite://{}?mode=rwc",
            home.path().join("scheduler.db").display()
        )
    } else {
        "sqlite::memory:".into()
    };
    let config: Config = bitrouter_sdk::config::parse_with(
        &r#"
server:
  skip_auth: true
database:
  url: "sqlite::memory:"
registry:
  inherit_defaults: false
providers:
  fixture:
    api_base: "http://127.0.0.1:1"
    api_key: fixture-only
    models: [{id: judge}]
"#
        .replace("sqlite::memory:", &database),
        |_| None,
    )?;
    let assembled =
        crate::assemble::build_app_with_path(&config, Some(&home.path().join("bitrouter.yaml")))
            .await?;
    let runtime = assembled.evolution;
    runtime
        .service("local")?
        .set_mode(
            mode,
            (mode == EvolutionMode::Automatic).then(|| "fixture:judge".into()),
        )
        .await?;
    let canonical = CanonicalStore::new(runtime.db.clone());
    let recorder = canonical
        .recorder(RecordingScope {
            owner: "local".into(),
            source: "fixture".into(),
            controller_instance_id: Some("controller".into()),
            route_scope_id: Some("local".into()),
        })
        .await?;
    runtime
        .inventory()
        .register_capture(recorder.connection_id(), "local", "controller")
        .await?;
    let identity = SessionIdentity {
        owner: "local".into(),
        source: "fixture".into(),
        native_session_id: "session".into(),
    };
    for (kind, method, call_id, payload) in [
        (
            CaptureKind::Request,
            "session/new",
            1,
            json!({"cwd":"/fixture"}),
        ),
        (
            CaptureKind::Response,
            "session/new",
            1,
            json!({"result":{"sessionId":"session"}}),
        ),
        (
            CaptureKind::Request,
            "session/prompt",
            2,
            json!({"sessionId":"session","prompt":[{"type":"text","text":"Fix the parser and run tests. No PR."}]}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Client,
                kind,
                call_id: Some(call_id),
                method: method.into(),
                payload,
            })
            .await?;
    }
    // Deliberately unknown fixture labels test scheduling, not judge accuracy.
    let unknown = crate::evolution::tests::unknown(&EvidencePacket {
        projection_version: crate::evolution::evidence::EVIDENCE_VERSION.into(),
        checkpoint_id: String::new(),
        prefix_digest: String::new(),
        gaps: vec![],
        items: vec![],
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let response = serde_json::to_string(&unknown)?;
    let app = App::builder()
        .language_model(|lm| {
            lm.routing_table(Arc::new(ConfigRoutingTable::from_config(config.clone())))
                .executor(Arc::new(MockExecutor::always_text(response)));
            lm.pre_request_hook(CountRequests(calls.clone()));
        })
        .build()?;
    let pipeline = app
        .language_model()
        .cloned()
        .context("judge pipeline missing")?;
    Ok(Fixture {
        runtime,
        canonical,
        recorder,
        identity,
        pipeline,
        calls,
        config,
        _home: home,
    })
}

async fn stop(fixture: &Fixture, call_id: u64) -> Result<()> {
    fixture
        .recorder
        .record(CaptureEvent {
            direction: CaptureDirection::Client,
            kind: CaptureKind::Response,
            call_id: Some(call_id),
            method: "session/prompt".into(),
            payload: json!({"result":{"stopReason":"end_turn"}}),
        })
        .await?;
    Ok(())
}

async fn continuation(fixture: &Fixture, call_id: u64) -> Result<()> {
    fixture.recorder.record(CaptureEvent { direction: CaptureDirection::Client, kind: CaptureKind::Request,
        call_id: Some(call_id), method: "session/prompt".into(), payload: json!({"sessionId":"session","prompt":[{"type":"text","text":"Continue the recorded fixture."}]}) }).await?;
    Ok(())
}

async fn discover(fixture: &Fixture) -> Result<JudgeJobs> {
    let service = fixture.runtime.service("local")?;
    let row = sessions::Entity::find_by_id(fixture.identity.key()?)
        .one(&fixture.runtime.db)
        .await?
        .context("session missing")?;
    EvolutionScheduler::new(fixture.runtime.clone())
        .discover(&service, &service.state().await?, &row)
        .await?;
    Ok(JudgeJobs::new(service.store))
}

#[tokio::test]
async fn automatic_stop_creates_one_checkpoint_and_one_job_across_restarts() -> Result<()> {
    let fixture = fixture(EvolutionMode::Automatic).await?;
    let worker = EvolutionScheduler::new(fixture.runtime.clone());
    assert!(
        fixture
            .canonical
            .checkpoints(&fixture.identity)
            .await?
            .is_empty()
    );
    assert_eq!(worker.tick(&fixture.pipeline).await?.checkpoints_created, 0);
    stop(&fixture, 2).await?;
    let report = worker.tick(&fixture.pipeline).await?;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!((report.checkpoints_created, report.jobs_completed), (1, 1));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(
        fixture
            .canonical
            .effective_assessment(&fixture.identity)
            .await?
            .assessment
            .is_some()
    );
    let restarted = EvolutionScheduler::new(fixture.runtime.clone());
    assert_eq!(restarted.tick(&fixture.pipeline).await?.jobs_completed, 0);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .canonical
            .checkpoints(&fixture.identity)
            .await?
            .len(),
        1
    );
    assert_eq!(
        fixture
            .canonical
            .assessment_history(&fixture.identity)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn manual_mode_freezes_without_a_model_and_auto_does_not_bulk_judge_old_stops() -> Result<()>
{
    let fixture = fixture(EvolutionMode::Manual).await?;
    stop(&fixture, 2).await?;
    let worker = EvolutionScheduler::new(fixture.runtime.clone());
    assert_eq!(worker.tick(&fixture.pipeline).await?.checkpoints_created, 1);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .canonical
            .effective_assessment(&fixture.identity)
            .await?
            .assessment
            .is_none()
    );
    fixture
        .runtime
        .service("local")?
        .set_mode(EvolutionMode::Automatic, Some("fixture:judge".into()))
        .await?;
    assert_eq!(worker.tick(&fixture.pipeline).await?.jobs_completed, 0);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    continuation(&fixture, 3).await?;
    stop(&fixture, 3).await?;
    assert_eq!(worker.tick(&fixture.pipeline).await?.jobs_completed, 1);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn off_retires_queued_automatic_work_without_dispatch() -> Result<()> {
    let fixture = fixture(EvolutionMode::Automatic).await?;
    stop(&fixture, 2).await?;
    let jobs = discover(&fixture).await?;
    fixture
        .runtime
        .service("local")?
        .set_mode(EvolutionMode::Off, None)
        .await?;
    let worker = EvolutionScheduler::new(fixture.runtime.clone());
    worker.tick(&fixture.pipeline).await?;
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(jobs.list().await?[0].status, JobStatus::Superseded);
    assert!(
        fixture
            .canonical
            .assessment_history(&fixture.identity)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn appended_work_supersedes_the_old_job_and_evaluates_only_the_new_stop() -> Result<()> {
    let fixture = fixture(EvolutionMode::Automatic).await?;
    stop(&fixture, 2).await?;
    let jobs = discover(&fixture).await?;
    continuation(&fixture, 3).await?;
    let worker = EvolutionScheduler::new(fixture.runtime.clone());
    worker.tick(&fixture.pipeline).await?;
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(jobs.list().await?[0].status, JobStatus::Superseded);
    stop(&fixture, 3).await?;
    assert_eq!(worker.tick(&fixture.pipeline).await?.jobs_completed, 1);
    assert_eq!(jobs.list().await?.len(), 2);
    assert_eq!(
        fixture
            .canonical
            .assessment_history(&fixture.identity)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_manual_revision_retires_queued_auto_work_and_is_never_overwritten() -> Result<()> {
    let fixture = fixture(EvolutionMode::Automatic).await?;
    stop(&fixture, 2).await?;
    let jobs = discover(&fixture).await?;
    let job = jobs.list().await?.remove(0);
    let prepared =
        scoring::prepare(&fixture.canonical, &fixture.identity, &job.checkpoint_id).await?;
    let receipt = scoring::submit(
        &fixture.canonical,
        &fixture.identity,
        scoring::RubricSubmission {
            submission_id: "operator-correction".into(),
            checkpoint_id: job.checkpoint_id,
            expected_revision: prepared.expected_revision,
            source: AssessmentSource::Human,
            evaluator_id: "operator".into(),
            evaluator_version: "v1".into(),
            evaluation: crate::evolution::tests::unknown(&prepared.evidence),
        },
    )
    .await?;
    EvolutionScheduler::new(fixture.runtime.clone())
        .tick(&fixture.pipeline)
        .await?;
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(jobs.list().await?[0].status, JobStatus::Superseded);
    assert_eq!(
        fixture
            .canonical
            .effective_assessment(&fixture.identity)
            .await?
            .current_revision,
        Some(receipt.revision.revision_id)
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_workers_share_one_judge_lease_and_one_canonical_revision() -> Result<()> {
    // Use the daemon's persistent storage boundary and separate connection
    // pools. Sharing an in-memory one-connection pool serializes access before
    // the durable lease is tested and loses its schema if that connection dies.
    let fixture = fixture_with_storage(EvolutionMode::Automatic, true).await?;
    stop(&fixture, 2).await?;
    let second = crate::assemble::build_app_with_path(
        &fixture.config,
        Some(&fixture._home.path().join("bitrouter.yaml")),
    )
    .await?;
    let left = EvolutionScheduler::new(fixture.runtime.clone());
    let right = EvolutionScheduler::new(second.evolution);
    let (a, b) = tokio::join!(left.tick(&fixture.pipeline), right.tick(&fixture.pipeline));
    a?;
    b?;
    left.tick(&fixture.pipeline).await?;
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .canonical
            .assessment_history(&fixture.identity)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn committed_assessment_recovery_is_metadata_only_even_after_mode_off() -> Result<()> {
    let fixture = fixture(EvolutionMode::Automatic).await?;
    stop(&fixture, 2).await?;
    let worker = EvolutionScheduler::new(fixture.runtime.clone());
    worker.tick(&fixture.pipeline).await?;
    let service = fixture.runtime.service("local")?;
    let jobs = JudgeJobs::new(service.store.clone());
    let job = jobs.list().await?.remove(0);
    let expected = job.outcome_revision.clone();
    let (revision, _): (_, super::super::jobs::JudgeJob) = service
        .store
        .get("judge_job", &job.job_id)
        .await?
        .context("judge job missing")?;
    service
        .store
        .update(
            "judge_job",
            &job.job_id,
            revision,
            |job: &mut super::super::jobs::JudgeJob| {
                job.status = JobStatus::ResponseStored;
                job.outcome_revision = None;
                Ok(())
            },
        )
        .await?;
    service.set_mode(EvolutionMode::Off, None).await?;
    worker.tick(&fixture.pipeline).await?;
    let recovered = jobs.get(&job.job_id).await?;
    assert_eq!(recovered.status, JobStatus::Completed);
    assert_eq!(recovered.outcome_revision, expected);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .canonical
            .assessment_history(&fixture.identity)
            .await?
            .len(),
        1
    );
    Ok(())
}

struct ParkJudge(Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl PreRequestHook for ParkJudge {
    async fn check(&self, _ctx: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn shutdown_cancels_background_work_and_preserves_the_uncertain_attempt() -> Result<()> {
    let fixture = fixture(EvolutionMode::Automatic).await?;
    stop(&fixture, 2).await?;
    let entered = Arc::new(tokio::sync::Notify::new());
    let app = App::builder()
        .language_model(|lm| {
            lm.routing_table(Arc::new(ConfigRoutingTable::from_config(
                fixture.config.clone(),
            )))
            .executor(Arc::new(MockExecutor::always_text("unused")));
            lm.pre_request_hook(CountRequests(fixture.calls.clone()));
            lm.pre_request_hook(ParkJudge(entered.clone()));
        })
        .build()?;
    let pipeline = app.language_model().cloned().context("pipeline missing")?;
    let stop = CancellationToken::new();
    let worker = EvolutionScheduler::new(fixture.runtime.clone());
    let cancellation = stop.clone();
    let task = tokio::spawn(async move { worker.run(pipeline, cancellation).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await?;
    stop.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(2), task).await??;
    let jobs = JudgeJobs::new(fixture.runtime.service("local")?.store);
    let job = jobs.list().await?.remove(0);
    assert_eq!(job.status, JobStatus::Running);
    assert_eq!(job.request_ids.len(), 1);
    assert!(job.lease_live()?);
    assert!(!fixture.runtime.worker_status().running);
    assert!(
        fixture
            .canonical
            .assessment_history(&fixture.identity)
            .await?
            .is_empty()
    );
    Ok(())
}
