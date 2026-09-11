use super::*;
use bitrouter_sdk::app::App;
use bitrouter_sdk::config::{Config, ConfigRoutingTable, ProviderConfig};
use bitrouter_sdk::language_model::context::PipelineContext;
use bitrouter_sdk::language_model::executor::MockExecutor;
use bitrouter_sdk::language_model::hooks::{HookDecision, PreRequestHook};
use bitrouter_sdk::language_model::types::ToolChoice;
use sea_orm::{DatabaseConnection, EntityTrait, Set};

use crate::acp_trajectory::{RecordingScope, SessionIdentity};
use bitrouter_sdk::acp::capture::{CaptureDirection, CaptureEvent, CaptureKind, CapturePort};

struct JudgeRequestContract;

#[async_trait::async_trait]
impl PreRequestHook for JudgeRequestContract {
    async fn check(&self, ctx: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        assert!(ctx.prompt().tools.is_empty());
        assert_eq!(ctx.prompt().tool_choice, Some(ToolChoice::None));
        assert!(
            ctx.prompt().params.max_tokens.is_none(),
            "judge has no product token ceiling"
        );
        assert!(!ctx.headers().contains_key("x-bitrouter-controller-id"));
        assert!(!ctx.headers().contains_key("x-bitrouter-acp-session-id"));
        Ok(HookDecision::Allow)
    }
}

fn pipeline(response: String) -> Result<Arc<Pipeline>> {
    pipeline_with_mode_switch(response, None)
}

struct SwitchEvolutionOff(crate::evolution::service::EvolutionService);

#[async_trait::async_trait]
impl PreRequestHook for SwitchEvolutionOff {
    async fn check(&self, _ctx: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        self.0
            .set_mode(crate::evolution::control::EvolutionMode::Off, None)
            .await
            .map_err(|error| bitrouter_sdk::error::BitrouterError::internal(error.to_string()))?;
        Ok(HookDecision::Allow)
    }
}

fn pipeline_with_mode_switch(
    response: String,
    switch: Option<crate::evolution::service::EvolutionService>,
) -> Result<Arc<Pipeline>> {
    let config = Config {
        providers: std::collections::HashMap::from([(
            "fixture".into(),
            ProviderConfig {
                api_base: "http://127.0.0.1:1".into(),
                active: true,
                ..ProviderConfig::default()
            },
        )]),
        ..Config::default()
    };
    let app = App::builder()
        .language_model(|lm| {
            lm.routing_table(Arc::new(ConfigRoutingTable::from_config(config)))
                .executor(Arc::new(MockExecutor::always_text(response)));
            lm.pre_request_hook(JudgeRequestContract);
            if let Some(service) = switch {
                lm.pre_request_hook(SwitchEvolutionOff(service));
            }
        })
        .build()?;
    app.language_model()
        .cloned()
        .context("missing fixture pipeline")
}

async fn setup() -> Result<(JudgeJobs, SessionIdentity, String)> {
    let db: DatabaseConnection = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let jobs = JudgeJobs::new(EvolutionStore::new(db, "local")?);
    let identity = SessionIdentity {
        owner: "local".into(),
        source: "fixture".into(),
        native_session_id: "job-session".into(),
    };
    let recorder = jobs
        .canonical
        .recorder(RecordingScope {
            owner: identity.owner.clone(),
            source: identity.source.clone(),
            controller_instance_id: None,
            route_scope_id: None,
        })
        .await?;
    for (kind, method, id, payload) in [
        (
            CaptureKind::Request,
            "session/new",
            1,
            serde_json::json!({"cwd":"/fixture"}),
        ),
        (
            CaptureKind::Response,
            "session/new",
            1,
            serde_json::json!({"result":{"sessionId":"job-session"}}),
        ),
        (
            CaptureKind::Request,
            "session/prompt",
            2,
            serde_json::json!({"sessionId":"job-session","prompt":[{"type":"text","text":"Fix the parser and run its tests. Do not create a PR."}]}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Client,
                kind,
                call_id: Some(id),
                method: method.into(),
                payload,
            })
            .await?;
    }
    let transcript = jobs.canonical.transcript(&identity).await?;
    let checkpoint = jobs
        .canonical
        .freeze_checkpoint(&identity, transcript.session.head)
        .await?;
    Ok((jobs, identity, checkpoint.checkpoint_id))
}

async fn evaluation(
    jobs: &JudgeJobs,
    identity: &SessionIdentity,
    checkpoint: &str,
) -> Result<super::super::rubric::RubricEvaluation> {
    let input = scoring::prepare(&jobs.canonical, identity, checkpoint).await?;
    Ok(super::super::tests::unknown(&input.evidence))
}

#[tokio::test]
async fn automatic_job_epoch_is_checked_before_any_model_attempt() -> Result<()> {
    use crate::evolution::{control::EvolutionMode, service::EvolutionService};
    let (jobs, identity, checkpoint) = setup().await?;
    let service = EvolutionService::new(jobs.store.db.clone(), "local")?;
    let state = service
        .set_mode(EvolutionMode::Automatic, Some("fixture:judge".into()))
        .await?;
    let job = jobs
        .enqueue(
            &identity,
            &checkpoint,
            "fixture:judge",
            Some(state.mode_epoch),
        )
        .await?;
    service
        .set_mode(EvolutionMode::Automatic, Some("fixture:new-judge".into()))
        .await?;
    assert!(
        jobs.run(&job.job_id, &pipeline("unused".into())?)
            .await
            .is_err()
    );
    assert!(jobs.get(&job.job_id).await?.request_ids.is_empty());
    assert!(
        jobs.canonical
            .assessment_history(&identity)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn obsolete_judge_input_is_retired_without_reserving_an_attempt() -> Result<()> {
    let (jobs, identity, checkpoint) = setup().await?;
    let job = jobs
        .enqueue(&identity, &checkpoint, "fixture:judge", None)
        .await?;
    let (revision, _): (_, JudgeJob) = jobs
        .store
        .get(KIND, &job.job_id)
        .await?
        .context("job missing")?;
    jobs.store
        .update(KIND, &job.job_id, revision, |stored: &mut JudgeJob| {
            stored.input_digest = "previous-evaluator-contract".into();
            Ok(())
        })
        .await?;
    assert!(
        jobs.run(&job.job_id, &pipeline("unused".into())?)
            .await
            .is_err()
    );
    let retired = jobs.get(&job.job_id).await?;
    assert_eq!(retired.status, JobStatus::Superseded);
    assert!(retired.request_ids.is_empty());
    assert_eq!(
        retired.error_code.as_deref(),
        Some("judge_contract_superseded")
    );
    assert!(
        jobs.canonical
            .assessment_history(&identity)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn off_during_a_model_call_fences_selection_and_keeps_the_charged_response() -> Result<()> {
    use crate::evolution::{control::EvolutionMode, service::EvolutionService};
    let (jobs, identity, checkpoint) = setup().await?;
    let service = EvolutionService::new(jobs.store.db.clone(), "local")?;
    let state = service
        .set_mode(EvolutionMode::Automatic, Some("fixture:judge".into()))
        .await?;
    let job = jobs
        .enqueue(
            &identity,
            &checkpoint,
            "fixture:judge",
            Some(state.mode_epoch),
        )
        .await?;
    let response = serde_json::to_string(&evaluation(&jobs, &identity, &checkpoint).await?)?;
    let pipeline = pipeline_with_mode_switch(response, Some(service))?;
    let error = jobs
        .run(&job.job_id, &pipeline)
        .await
        .err()
        .context("inactive epoch unexpectedly selected an assessment")?;
    assert!(
        error.to_string().contains("inactive feedback epoch"),
        "unexpected failure: {error:#}"
    );
    let failed = jobs.get(&job.job_id).await?;
    assert_eq!(failed.status, JobStatus::Failed);
    assert!(failed.cached_response.is_some());
    assert_eq!(failed.request_ids.len(), 1);
    assert!(
        jobs.canonical
            .assessment_history(&identity)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn completed_job_is_idempotent_without_another_model_call() -> Result<()> {
    let (jobs, identity, checkpoint) = setup().await?;
    let job = jobs
        .enqueue(&identity, &checkpoint, "fixture:judge", None)
        .await?;
    let pipeline = pipeline(serde_json::to_string(
        &evaluation(&jobs, &identity, &checkpoint).await?,
    )?)?;
    let completed = jobs.run(&job.job_id, &pipeline).await?;
    assert_eq!(completed.status, JobStatus::Completed);
    assert_eq!(completed.request_ids.len(), 1);
    let repeated = jobs
        .enqueue(&identity, &checkpoint, "fixture:judge", None)
        .await?;
    assert_eq!(
        job.job_id, repeated.job_id,
        "a new effective revision is not a new job"
    );
    assert_eq!(jobs.run(&job.job_id, &pipeline).await?.request_ids.len(), 1);
    assert_eq!(jobs.canonical.assessment_history(&identity).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn stopping_lease_renewal_drains_database_io_and_preserves_the_memory_database() -> Result<()>
{
    let (jobs, identity, checkpoint) = setup().await?;
    let job = jobs
        .enqueue(&identity, &checkpoint, "fixture:judge", None)
        .await?;
    let (revision, _): (_, JudgeJob) = jobs
        .store
        .get(KIND, &job.job_id)
        .await?
        .context("missing job")?;
    jobs.store
        .update(KIND, &job.job_id, revision, |stored: &mut JudgeJob| {
            stored.lease_owner = Some("renewal-test".into());
            Ok(())
        })
        .await?;
    let (claimed_revision, _): (_, JudgeJob) = jobs
        .store
        .get(KIND, &job.job_id)
        .await?
        .context("missing claimed job")?;

    // Hold the pool's only connection so renewal is suspended in acquisition.
    // Poll explicitly: a stop must wait for that operation, not drop its future.
    let transaction = jobs.store.db.begin().await?;
    tokio::time::pause();
    let stop = tokio_util::sync::CancellationToken::new();
    let renewal = jobs.renew_lease(&job.job_id, "renewal-test", &stop);
    tokio::pin!(renewal);
    assert!(futures::poll!(&mut renewal).is_pending());
    // Pass the timer wheel's rounded deadline, rather than landing on it.
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    assert!(futures::poll!(&mut renewal).is_pending());
    stop.cancel();
    assert!(futures::poll!(&mut renewal).is_pending());
    tokio::time::resume();
    transaction.commit().await?;
    renewal.await?;

    let (renewed_revision, renewed): (_, JudgeJob) = jobs
        .store
        .get(KIND, &job.job_id)
        .await?
        .context("renewed job missing")?;
    assert_eq!(renewed_revision, claimed_revision + 1);
    assert!(renewed.lease_until.is_some());
    assert_eq!(
        jobs.canonical
            .checkpoint_content(&identity, &checkpoint)
            .await?
            .checkpoint
            .checkpoint_id,
        checkpoint
    );
    Ok(())
}

#[tokio::test]
async fn cached_response_survives_restart_without_calling_the_model() -> Result<()> {
    let (jobs, identity, checkpoint) = setup().await?;
    let mut job = jobs
        .enqueue(&identity, &checkpoint, "fixture:judge", None)
        .await?;
    job.status = JobStatus::ResponseStored;
    job.request_ids = vec!["already-charged".into()];
    job.cached_response = Some(judge::JudgeOutput {
        request_id: "already-charged".into(),
        model: job.model.clone(),
        judge_version: judge::JUDGE_VERSION.into(),
        input_digest: job.input_digest.clone(),
        usage: None,
        evaluation: evaluation(&jobs, &identity, &checkpoint).await?,
    });
    let (revision, _): (_, JudgeJob) = jobs
        .store
        .get(KIND, &job.job_id)
        .await?
        .context("missing job")?;
    jobs.store
        .update(KIND, &job.job_id, revision, |stored: &mut JudgeJob| {
            *stored = job.clone();
            Ok(())
        })
        .await?;
    let restarted = JudgeJobs::new(jobs.store.clone());
    // Invalid model output would fail if restart incorrectly invoked the model.
    let done = restarted
        .run(&job.job_id, &pipeline("this is not a rubric".into())?)
        .await?;
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.request_ids, ["already-charged"]);
    Ok(())
}

#[tokio::test]
async fn a_late_job_cannot_replace_a_manual_correction() -> Result<()> {
    let (jobs, identity, checkpoint) = setup().await?;
    let job = jobs
        .enqueue(&identity, &checkpoint, "fixture:judge", None)
        .await?;
    let eval = evaluation(&jobs, &identity, &checkpoint).await?;
    let manual = scoring::submit(
        &jobs.canonical,
        &identity,
        scoring::RubricSubmission {
            submission_id: "manual".into(),
            checkpoint_id: checkpoint.clone(),
            expected_revision: None,
            source: AssessmentSource::Human,
            evaluator_id: "operator".into(),
            evaluator_version: "v1".into(),
            evaluation: eval.clone(),
        },
    )
    .await?;
    assert!(
        jobs.run(&job.job_id, &pipeline(serde_json::to_string(&eval)?)?)
            .await
            .is_err()
    );
    let failed = jobs.get(&job.job_id).await?;
    assert_eq!(failed.status, JobStatus::Failed);
    assert!(failed.cached_response.is_some());
    assert_eq!(
        jobs.canonical
            .effective_assessment(&identity)
            .await?
            .current_revision,
        Some(manual.revision.revision_id)
    );
    assert_eq!(jobs.canonical.assessment_history(&identity).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn source_deletion_removes_cached_judge_text_and_disallows_resume() -> Result<()> {
    let (jobs, identity, checkpoint) = setup().await?;
    let job = jobs
        .enqueue(&identity, &checkpoint, "fixture:judge", None)
        .await?;
    jobs.run(
        &job.job_id,
        &pipeline(serde_json::to_string(
            &evaluation(&jobs, &identity, &checkpoint).await?,
        )?)?,
    )
    .await?;
    jobs.canonical.delete(&identity).await?;
    assert!(jobs.get(&job.job_id).await.is_err());
    assert!(jobs.list().await?.is_empty());
    assert!(
        jobs.run(&job.job_id, &pipeline("unused".into())?)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn a_live_lease_prevents_a_second_worker_and_an_expired_lease_recovers() -> Result<()> {
    let (jobs, identity, checkpoint) = setup().await?;
    let job = jobs
        .enqueue(&identity, &checkpoint, "fixture:judge", None)
        .await?;
    let record_id = jobs.store.id(KIND, &job.job_id)?;
    let mut stored: super::super::store::records::ActiveModel =
        super::super::store::records::Entity::find_by_id(&record_id)
            .one(&jobs.store.db)
            .await?
            .context("missing record")?
            .into();
    let mut leased = job.clone();
    leased.lease_owner = Some("crashed-worker".into());
    leased.lease_until = Some((Utc::now() + chrono::Duration::minutes(1)).to_rfc3339());
    stored.body = Set(serde_json::to_string(&leased)?);
    use sea_orm::ActiveModelTrait;
    stored.update(&jobs.store.db).await?;
    let pipeline = pipeline(serde_json::to_string(
        &evaluation(&jobs, &identity, &checkpoint).await?,
    )?)?;
    assert!(jobs.run(&job.job_id, &pipeline).await.is_err());
    assert!(jobs.get(&job.job_id).await?.request_ids.is_empty());
    let (revision, _): (_, JudgeJob) = jobs
        .store
        .get(KIND, &job.job_id)
        .await?
        .context("missing job")?;
    jobs.store
        .update(KIND, &job.job_id, revision, |stored: &mut JudgeJob| {
            stored.lease_until = Some((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339());
            Ok(())
        })
        .await?;
    assert_eq!(
        jobs.run(&job.job_id, &pipeline).await?.status,
        JobStatus::Completed
    );
    Ok(())
}
