//! Actual public in-process consumer. Requires the separately built ACP peer.
use super::*;
use crate::{
    acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore, RevisionInput},
    core::evolution::{
        rubric::{self, Applicability, RubricEvaluation, RubricItem},
        scoring::RubricSubmission,
    },
    harness::acp::controlled::CheckpointReview,
    storage::operation_budget::Error as BudgetError,
};
mod handoff;
mod limits;
mod required;

struct Host {
    ready: Ready,
    judge: Judge,
    provider: Provider,
    listener: Listener,
    app: Arc<App>,
    directory: tempfile::TempDir,
    marker: PathBuf,
    session: Option<ControlledSession>,
}
impl Host {
    async fn new(profile: bool, hold_from: usize) -> Result<Self> {
        Self::in_directory(profile, hold_from, tempfile::tempdir()?).await
    }
    async fn in_directory(
        profile: bool,
        hold_from: usize,
        directory: tempfile::TempDir,
    ) -> Result<Self> {
        let path = directory.path().join("owned-human.sqlite");
        let owner = crate::tenancy::ownership::acquire_path(&path, true)?;
        let db =
            crate::db::connect_tenancy(&format!("sqlite://{}", path.display()), &owner).await?;
        let fixture = Fixture::with_database(db, OperationLimits::default(), &mut |_| {}).await?;
        let provider = Provider::start(false).await?;
        let judge = Judge::hold_from(hold_from).await?;
        let ready = Ready::from_fixture(
            fixture,
            Some(owner),
            &provider.url,
            &judge.url,
            profile,
            LearningMode::Observe,
            2,
        )
        .await?;
        ensure!(
            std::fs::metadata(&path)?.len() > 0,
            "physical fixture missing"
        );
        let app = Arc::new(
            host_app(
                &ready.fixture,
                &provider.url,
                Arc::new(HostSpies::default()),
            )?
            .with_tenancy(ready.fixture.service.clone()),
        );
        let listener = Listener::start(app.clone()).await?;
        let marker = directory.path().join("human-child.json");
        Ok(Self {
            ready,
            judge,
            provider,
            listener,
            app,
            directory,
            marker,
            session: None,
        })
    }
    async fn open(&mut self) -> Result<String> {
        self.session = Some(
            ControlledSession::open_authenticated(
                self.app.clone(),
                self.ready.headers()?,
                launch(&executable()?, &self.marker, &self.listener.origin, None),
                CancellationToken::new(),
            )
            .await?,
        );
        self.current()?
            .new_session(self.directory.path().to_owned())
            .await
    }
    fn current(&self) -> Result<&ControlledSession> {
        self.session.as_ref().context("real controller absent")
    }
    async fn finish(mut self, result: Result<()>) -> Result<()> {
        self.ready.fixture.service.cleanup.release_gate();
        self.judge.release.cancel();
        let controller = match self.session.take() {
            Some(session) => session.shutdown().await,
            None => Ok(()),
        };
        let stopped = self.listener.finish().await;
        let service = tokio::time::timeout(TIMEOUT, self.ready.fixture.service.shutdown())
            .await
            .context("human fixture service cleanup deadline")
            .and_then(|value| value);
        // No absent-marker success: this fixture has an actual opener on every
        // tested path, and service drain must precede the real process check.
        let child = reaped(&self.marker).await;
        let judge = self.judge.stop().await;
        let provider = self.provider.finish().await;
        let db = self
            .ready
            .fixture
            .db
            .clone()
            .close()
            .await
            .map_err(anyhow::Error::from);
        result
            .and(controller)
            .and(stopped)
            .and(service)
            .and(child)
            .and(judge)
            .and(provider)
            .and(db)
    }
}

fn evaluation() -> RubricEvaluation {
    RubricEvaluation {
        rubric_version: rubric::RUBRIC_VERSION.into(),
        items: rubric::library()
            .into_iter()
            .map(|criterion| RubricItem {
                criterion_id: criterion.id.into(),
                applicability: Applicability::Unknown,
                selection_reason: "Explicit manual review of bounded recorded evidence".into(),
                score: CriterionScore::Unknown,
                evidence: vec![],
                explanation: "No unobserved success is claimed".into(),
            })
            .collect(),
        diagnostics: vec![],
        severe_violation: false,
        violation_evidence: vec![],
        summary: "Human reviewed the recorded task".into(),
    }
}
fn submission(view: &CheckpointReview, id: &str) -> RubricSubmission {
    RubricSubmission {
        submission_id: id.into(),
        checkpoint_id: view.checkpoint.checkpoint_id.clone(),
        expected_revision: view.scoring.expected_revision.clone(),
        source: AssessmentSource::Human,
        evaluator_id: "caller-cannot-pick-this-principal".into(),
        evaluator_version: "ignored-label".into(),
        evaluation: evaluation(),
    }
}
fn rejected(error: &anyhow::Error, code: ErrorCode) -> Result<()> {
    ensure!(
        matches!(error.downcast_ref::<BudgetError>(), Some(BudgetError::Rejected(actual)) if *actual == code),
        "expected typed {code:?}, got {:?}: {error:#}",
        error.downcast_ref::<BudgetError>()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn public_human_correction_and_retraction_win_held_judge_without_losing_paid_facts()
-> Result<()> {
    for retract in [false, true] {
        let mut host = Host::new(true, 2).await?;
        let result = async {
            let native = host.open().await?;
            let current = host.current()?;
            current.prompt(&native, "first stopped task").await?;
            current.checkpoint(&native).await?.context("first checkpoint")?;
            drained(&host.ready).await?;
            let first = current.review_checkpoint(&native, None).await?;
            ensure!(first.history.len() == 1 && first.history[0].input.source == AssessmentSource::Agentic);
            let paid = current.checkpoint(&native);
            // Duplicate A1 does not create another provider request.
            paid.await?;
            current.prompt(&native, "second stopped task").await?;
            drained(&host.ready).await?;
            let work = current.checkpoint(&native);
            tokio::pin!(work);
            tokio::select! {
                reply = &mut work => anyhow::bail!("second judge was not held: {reply:?}"),
                result = tokio::time::timeout(TIMEOUT, async {
                    while host.judge.calls.load(Ordering::SeqCst) < 2 { tokio::task::yield_now().await; }
                }) => { result?; }
            }
            let second = current.review_checkpoint(&native, None).await?;
            ensure!(second.checkpoint.checkpoint_id != first.checkpoint.checkpoint_id
                && second.scoring.expected_revision == first.scoring.expected_revision);
            // Ordinary client command capacity remains occupied by the judge.
            ensure!(current.prompt(&native, "must not be forwarded").await.is_err());
            let input = submission(&second, "human-second");
            let human = current.submit_assessment(&native, input.clone()).await?;
            ensure!(human.revision.selected_on_submission
                && human.revision.input.evaluator_id.starts_with("owned-human/")
                && human.revision.input.evaluator_id != input.evaluator_id
                && human.revision.input.evaluator_version == "owned-manual-rubric-v1");
            let mut alias_retry = input;
            alias_retry.evaluator_id = String::new();
            alias_retry.evaluator_version = "another bounded alias".into();
            let replay = current.submit_assessment(&native, alias_retry).await?;
            ensure!(serde_json::to_value(&replay)? == serde_json::to_value(&human)?);
            let selected = if retract {
                let reviewed = current.review_checkpoint(&native, None).await?;
                current.retract_assessment(&native, RevisionInput {
                    submission_id: "human-retract".into(), checkpoint_id: reviewed.checkpoint.checkpoint_id,
                    expected_revision: reviewed.scoring.expected_revision, source: AssessmentSource::Human,
                    evaluator_id: String::new(), evaluator_version: String::new(), assessment: None,
                    reason: "Explicitly withdraw this assessment".into(),
                }).await?
            } else { human.revision };
            host.judge.release.cancel();
            tokio::time::timeout(TIMEOUT, &mut work).await??;
            drained(&host.ready).await?;
            let final_view = current.review_checkpoint(&native, None).await?;
            ensure!(final_view.scoring.expected_revision.as_deref() == Some(selected.revision_id.as_str())
                && final_view.prefill_revision.as_deref() == Some(selected.revision_id.as_str())
                && final_view.previous.is_none() == retract);
            let automatic = final_view.history.iter().find(|value| value.input.source == AssessmentSource::Agentic)
                .context("paid terminal assessment missing")?;
            ensure!(!automatic.selected_on_submission);
            let jobs = bodies(&host.ready.fixture, "owned_judge_job").await?;
            let second_job = jobs.iter().find(|value| value["checkpoint"] == final_view.checkpoint.checkpoint_id)
                .context("second paid job")?;
            ensure!(second_job["status"] == "unselected" && second_job["output"]["usage"]["prompt_tokens"] == 4
                && second_job["history_reservation"]["state"]["state"] == "consumed");
            let receipt = receipt(&host.ready, second_job["execution"].as_str().context("execution")?).await?;
            ensure!(receipt["usage"]["input_tokens"] == 4 && receipt["usage"]["output_tokens"] == 2);
            ensure!(bodies(&host.ready.fixture, "owned_judge_budget").await?[0]["accepted"] == 2
                && bodies(&host.ready.fixture, "owned_judge_pending").await?.is_empty()
                && host.judge.calls.load(Ordering::SeqCst) == 2);
            Ok(())
        }.await;
        host.finish(result).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn public_manual_no_paid_profile_exact_replay_and_stale_head_leave_controller_healthy()
-> Result<()> {
    let mut host = Host::new(false, usize::MAX).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        ensure!(
            current
                .review_checkpoint("guessed native", None)
                .await
                .is_err()
        );
        let missing = current
            .review_checkpoint(&native, None)
            .await
            .err()
            .context("review unexpectedly froze")?;
        rejected(&missing, ErrorCode::NotFound)?;
        current.prompt(&native, "bounded manual task").await?;
        current.checkpoint(&native).await?.context("checkpoint")?;
        let view = current.review_checkpoint(&native, None).await?;
        let one = submission(&view, "manual-one");
        let accepted = current.submit_assessment(&native, one.clone()).await?;
        let stale = current
            .submit_assessment(&native, submission(&view, "manual-stale"))
            .await
            .err()
            .context("stale head accepted")?;
        rejected(&stale, ErrorCode::RevisionConflict)?;
        let mut wrong = one.clone();
        wrong.evaluation.summary = "changed real content".into();
        rejected(
            &current
                .submit_assessment(&native, wrong)
                .await
                .err()
                .context("conflicting replay accepted")?,
            ErrorCode::RevisionConflict,
        )?;
        let mut agent = one;
        agent.source = AssessmentSource::Agentic;
        rejected(
            &current
                .submit_assessment(&native, agent)
                .await
                .err()
                .context("Agentic manual request accepted")?,
            ErrorCode::InvalidRequest,
        )?;
        let after = current.review_checkpoint(&native, None).await?;
        ensure!(
            after.history.len() == 1
                && after.scoring.expected_revision == Some(accepted.revision.revision_id)
        );
        ensure!(
            host.ready.fixture.service.current().is_ok()
                && bodies(&host.ready.fixture, "owned_judge_job")
                    .await?
                    .is_empty()
                && bodies(&host.ready.fixture, "owned_judge_budget")
                    .await?
                    .is_empty()
        );
        current
            .prompt(&native, "ordinary callback still works")
            .await?;
        Ok(())
    }
    .await;
    host.finish(result).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn same_database_equal_native_names_do_not_adopt_another_controller_history() -> Result<()> {
    let mut host = Host::new(false, usize::MAX).await?;
    let mut other = None;
    let marker = host.directory.path().join("other-controller.json");
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "first controller").await?;
        let first = current.checkpoint(&native).await?.context("checkpoint")?;
        let a = current.review_checkpoint(&native, None).await?;
        let one = current
            .submit_assessment(&native, submission(&a, "same-submission"))
            .await?;
        ensure!(
            current
                .review_checkpoint(&native, None)
                .await?
                .history
                .len()
                == 1
        );
        // The Service owns one authenticated host slot. Reuse the same database
        // only after the original real controller and child have fully stopped.
        // Its immutable assessment must remain unchanged by the next controller.
        host.session
            .take()
            .context("original checked controller")?
            .shutdown()
            .await?;
        reaped(&host.marker).await?;
        other = Some(
            ControlledSession::open_authenticated(
                host.app.clone(),
                host.ready.headers()?,
                launch(&executable()?, &marker, &host.listener.origin, None),
                CancellationToken::new(),
            )
            .await?,
        );
        let second = other.as_ref().context("second checked controller")?;
        ensure!(
            second
                .review_checkpoint(&native, Some(&first.checkpoint_id))
                .await
                .is_err(),
            "membership was inferred from caller/ACP text"
        );
        let second_native = second.new_session(host.directory.path().to_owned()).await?;
        ensure!(
            native == second_native,
            "fixture must exercise equal public native IDs"
        );
        rejected(
            &second
                .review_checkpoint(&second_native, Some(&first.checkpoint_id))
                .await
                .err()
                .context("foreign controller checkpoint was returned")?,
            ErrorCode::NotFound,
        )?;
        second.prompt(&second_native, "second controller").await?;
        second.checkpoint(&second_native).await?;
        let b = second.review_checkpoint(&second_native, None).await?;
        ensure!(a.checkpoint.checkpoint_id != b.checkpoint.checkpoint_id);
        let two = second
            .submit_assessment(&second_native, submission(&b, "same-submission"))
            .await?;
        ensure!(
            one.revision.revision_id != two.revision.revision_id
                && second
                    .review_checkpoint(&second_native, None)
                    .await?
                    .history
                    .len()
                    == 1
                && host.ready.fixture.service.current().is_ok()
        );
        // Read back the two actual actor-written rows; no fixture write or
        // fabricated association substitutes for either public submission.
        let retained = host
            .ready
            .fixture
            .db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT revision_json FROM acp_assessment_revisions".to_owned(),
            ))
            .await?
            .into_iter()
            .map(|row| {
                Ok(serde_json::from_str::<Value>(
                    &row.try_get::<String>("", "revision_json")?,
                )?)
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            retained.len() == 2
                && retained.contains(&serde_json::to_value(&one.revision)?)
                && retained.contains(&serde_json::to_value(&two.revision)?),
            "a new controller changed or adopted the previous assessment"
        );
        Ok(())
    }
    .await;
    let closed = match other {
        Some(session) => session.shutdown().await,
        None => Ok(()),
    };
    let child = if marker.exists() {
        reaped(&marker).await
    } else {
        Err(anyhow::anyhow!(
            "second actual child was not started/reaped"
        ))
    };
    host.finish(result.and(closed).and(child)).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn historical_public_correction_retains_its_prefix_without_overwriting_a_newer_head()
-> Result<()> {
    let mut host = Host::new(false, usize::MAX).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "prefix one").await?;
        let first = current
            .checkpoint(&native)
            .await?
            .context("first checkpoint")?;
        current.prompt(&native, "prefix two").await?;
        current.checkpoint(&native).await?;
        let second = current.review_checkpoint(&native, None).await?;
        let selected = current
            .submit_assessment(&native, submission(&second, "newer-head"))
            .await?;
        let historical = current
            .review_checkpoint(&native, Some(&first.checkpoint_id))
            .await?;
        ensure!(
            historical.checkpoint.watermark < historical.current_watermark
                && historical.scoring.expected_revision.as_deref()
                    == Some(selected.revision.revision_id.as_str())
        );
        let old = current
            .submit_assessment(&native, submission(&historical, "historical-review"))
            .await?;
        ensure!(!old.revision.selected_on_submission && old.revision.supersedes.is_none());
        let after = current.review_checkpoint(&native, None).await?;
        ensure!(after.scoring.expected_revision == Some(selected.revision.revision_id));
        let invalid = RevisionInput {
            submission_id: "wrong-checkpoint-retract".into(),
            checkpoint_id: first.checkpoint_id,
            expected_revision: after.scoring.expected_revision,
            source: AssessmentSource::Human,
            evaluator_id: String::new(),
            evaluator_version: String::new(),
            assessment: None,
            reason: "A caller cannot retract an unrelated selected checkpoint".into(),
        };
        rejected(
            &current
                .retract_assessment(&native, invalid)
                .await
                .err()
                .context("foreign head retracted")?,
            ErrorCode::RevisionConflict,
        )?;
        ensure!(count(&host.ready.fixture, "acp_assessment_revisions").await? == 2);
        Ok(())
    }
    .await;
    host.finish(result).await
}
