//! Bounded startup recovery and known-versus-unknown review failures.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_session_recovery_exhausts_its_bound_without_prompt_or_delivery() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_FAIL_INITIALIZE");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        let exhausted = wait_for(&fixture.socket, &id, |goal| {
            goal.workflow
                .tasks
                .iter()
                .filter(|task| task.spec.dependencies.is_empty())
                .count()
                == 2
                && goal
                    .workflow
                    .tasks
                    .iter()
                    .filter(|task| task.spec.dependencies.is_empty())
                    .all(|task| {
                        task.executions.len() == 3
                            && task
                                .executions
                                .last()
                                .is_some_and(|execution| execution.effects_reconciled())
                    })
        })
        .await?;
        tokio::time::sleep(Duration::from_millis(2_100)).await;
        let held = head(&fixture.socket, &id).await?;
        assert_eq!(held.requirements_revision, 1);
        assert_eq!(held.run_intent, RunIntent::Run);
        assert_eq!(
            held.workflow
                .tasks
                .iter()
                .map(|task| task.executions.len())
                .sum::<usize>(),
            6
        );
        assert!(!held.workflow.completion.completed_for(1));
        assert!(
            held.workflow
                .tasks
                .iter()
                .filter(|task| !task.spec.dependencies.is_empty())
                .all(|task| task.executions.is_empty())
        );
        assert_eq!(held.budget.limits, exhausted.budget.limits);
        let mut files = tokio::fs::read_dir(&fixture.captures).await?;
        let mut starts = 0;
        while let Some(file) = files.next_entry().await? {
            let text = tokio::fs::read_to_string(file.path()).await?;
            for line in text.lines() {
                let message: serde_json::Value = serde_json::from_str(line)?;
                starts += usize::from(message["method"] == "initialize");
                assert_ne!(message["method"], "session/new");
                assert_ne!(message["method"], "session/prompt");
            }
        }
        assert_eq!(starts, 6);

        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_pause_prevents_pre_session_recovery_from_admitting_new_work() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_FAIL_FIRST_INITIALIZE");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        let failed = wait_for(&fixture.socket, &id, |goal| {
            goal.workflow
                .tasks
                .iter()
                .flat_map(|task| &task.executions)
                .any(|execution| {
                    matches!(
                        execution.observation,
                        Observation::Unknown {
                            run_id: Some(_),
                            ..
                        } | Observation::Failed {
                            run_id: Some(_),
                            ..
                        }
                    )
                })
        })
        .await?;
        let failed_id = failed
            .workflow
            .tasks
            .iter()
            .flat_map(|task| &task.executions)
            .find(|execution| {
                matches!(
                    execution.observation,
                    Observation::Unknown {
                        run_id: Some(_),
                        ..
                    } | Observation::Failed {
                        run_id: Some(_),
                        ..
                    }
                )
            })
            .context("failed execution missing")?
            .id
            .clone();
        control(
            &fixture.socket,
            &id,
            "user-pause-before-retry",
            RunIntent::Pause,
        )
        .await?;
        let paused = wait_for(&fixture.socket, &id, |goal| {
            goal.run_intent == RunIntent::Pause
                && goal
                    .workflow
                    .tasks
                    .iter()
                    .flat_map(|task| &task.executions)
                    .find(|execution| execution.id == failed_id)
                    .is_some_and(|execution| {
                        matches!(execution.stop, Some(StopState::Confirmed { .. }))
                    })
        })
        .await?;
        let attempts = paused
            .workflow
            .tasks
            .iter()
            .map(|task| task.executions.len())
            .sum::<usize>();
        tokio::time::sleep(Duration::from_millis(2_100)).await;
        let held = head(&fixture.socket, &id).await?;
        assert_eq!(held.run_intent, RunIntent::Pause);
        assert_eq!(held.requirements_revision, 1);
        assert_eq!(
            held.workflow
                .tasks
                .iter()
                .map(|task| task.executions.len())
                .sum::<usize>(),
            attempts
        );
        assert!(!held.workflow.completion.completed_for(1));

        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_pre_session_failure_retries_without_replaying_goal_work() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_FAIL_FIRST_INITIALIZE");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        fixture.release().await?;
        let completed = fixture.completed(&id, 1).await?;
        let root = fixture.root(&id);
        let attempts = completed
            .workflow
            .tasks
            .iter()
            .flat_map(|task| &task.executions)
            .collect::<Vec<_>>();
        assert_eq!(
            attempts.len(),
            4,
            "one startup failed, three real task prompts remain"
        );
        let failed = attempts
            .iter()
            .filter(|execution| {
                matches!(
                    execution.observation,
                    Observation::Unknown { .. } | Observation::Failed { .. }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].effects_reconciled());
        let Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Verified {
            transcript,
            ..
        }) = &failed[0].reconciliation
        else {
            anyhow::bail!("original startup reconciliation missing")
        };
        let evidence: serde_json::Value =
            serde_json::from_slice(&crate::goal_artifacts::read(&root, transcript).await?)?;
        assert_eq!(evidence["snapshot"]["process"], "failed");
        assert_eq!(evidence["snapshot"]["startup_aborted_before_session"], true);
        assert!(evidence["history"]["history_complete"].as_bool() == Some(true));
        let mut files = tokio::fs::read_dir(&fixture.captures).await?;
        let (mut failed_starts, mut prompts) = (0, 0);
        while let Some(file) = files.next_entry().await? {
            let text = tokio::fs::read_to_string(file.path()).await?;
            let messages = text
                .lines()
                .map(serde_json::from_str::<serde_json::Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let opened = messages
                .iter()
                .any(|message| message["method"] == "session/new");
            let admitted = messages
                .iter()
                .filter(|message| message["method"] == "session/prompt")
                .count();
            prompts += admitted;
            if !opened {
                assert_eq!(admitted, 0);
                failed_starts += 1;
            }
        }
        assert_eq!((failed_starts, prompts), (1, 3));
        Ok(())
    }
    .await;
    fixture.finish(result).await
}
async fn assert_known_repair(
    fixture: &Fixture,
    id: &str,
    running: &GoalSnapshot,
    failure_marker: &str,
) -> Result<()> {
    let completed = fixture.completed(id, 1).await?;
    let root = fixture.root(id);
    let left = completed
        .workflow
        .tasks
        .iter()
        .find(|task| task.spec.id == "left")
        .context("left missing")?;
    assert_eq!(left.requirements_revision, 1);
    assert_eq!(left.executions.len(), 2);
    assert!(left.executions[0].effects_reconciled());
    assert!(left.executions[0].handoff.is_none());
    assert_eq!(
        completed
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "right")
            .context("right missing")?
            .executions
            .len(),
        1
    );
    let prepared = left.executions[1]
        .resources
        .as_ref()
        .context("repair prompt missing")?;
    let prompt = String::from_utf8(crate::goal_artifacts::read(&root, &prepared.prompt).await?)?;
    assert!(prompt.contains(&left.executions[0].id));
    assert!(prompt.contains("failed_candidate_material") && prompt.contains("actual_checks"));
    assert!(prompt.contains(failure_marker));
    assert_eq!(completed.budget.limits, running.budget.limits);
    fixture.left_history().await?;
    Ok(())
}

fn rejected_review() -> wiremock::ResponseTemplate {
    wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id":"fixture-rejection","object":"chat.completion","model":"test-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":
            serde_json::json!({"ready":false,"reason":"FIXTURE_REPAIR_REASON: a known semantic defect remains","missing_checks":[]}).to_string()},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}))
}

fn is_left_review(request: &wiremock::Request) -> bool {
    provider::model_state(request).is_some_and(|state| {
        state.get("actual_workspace").is_some() && state["task"]["id"] == "left"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn known_check_failure_repairs_with_actual_evidence_and_same_requirements() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_FAIL_LEFT_ONCE");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;
        let running = fixture.running(&id).await?;
        fixture.release().await?;
        assert_known_repair(&fixture, &id, &running, "WRONG").await?;
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn known_review_failure_repairs_without_restarting_independent_work() -> Result<()> {
    let setup = Setup::new().await?;
    let reviews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            if is_left_review(request)
                && reviews.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
            {
                rejected_review()
            } else {
                provider::model(request)
            }
        })
        .with_priority(1)
        .mount(&setup.backend)
        .await;

    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;
        let running = fixture.running(&id).await?;
        fixture.release().await?;
        assert_known_repair(&fixture, &id, &running, "FIXTURE_REPAIR_REASON").await?;
        Ok(())
    }
    .await;
    fixture.finish(result).await
}
async fn assert_review_does_not_retry(
    fixture: &Fixture,
    id: &str,
    blocked: &GoalSnapshot,
    limit: usize,
) -> Result<()> {
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let held = head(&fixture.socket, id).await?;
    assert_eq!(held.requirements_revision, 1);
    assert_eq!(held.run_intent, RunIntent::Run);
    assert_eq!(held.budget.limits, blocked.budget.limits);
    assert_eq!(
        held.workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "left")
            .context("left missing")?
            .executions
            .len(),
        limit
    );
    assert!(
        held.workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "join")
            .context("join missing")?
            .executions
            .is_empty()
    );
    assert!(!held.workflow.completion.completed_for(1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_review_failure_stops_at_three_executions_per_version() -> Result<()> {
    let setup = Setup::new().await?;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            if is_left_review(request) {
                rejected_review()
            } else {
                provider::model(request)
            }
        })
        .with_priority(1)
        .mount(&setup.backend)
        .await;

    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        fixture.release().await?;
        let blocked = wait_for(&fixture.socket, &id, |goal| {
            goal.workflow
                .tasks
                .iter()
                .find(|task| task.spec.id == "left")
                .is_some_and(|task| {
                    task.executions.len() == 3
                        && task.executions.last().is_some_and(|execution| {
                            matches!(execution.review, Some(ReviewState::Rejected { .. }))
                                && execution.effects_reconciled()
                        })
                })
        })
        .await?;
        assert_review_does_not_retry(&fixture, &id, &blocked, 3).await?;

        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_review_outcome_never_authorizes_a_repair() -> Result<()> {
    let setup = Setup::new().await?;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            if is_left_review(request) {
                wiremock::ResponseTemplate::new(500)
            } else {
                provider::model(request)
            }
        })
        .with_priority(1)
        .mount(&setup.backend)
        .await;

    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        fixture.release().await?;
        let blocked = wait_for(&fixture.socket, &id, |goal| {
            goal.workflow
                .tasks
                .iter()
                .find(|task| task.spec.id == "left")
                .is_some_and(|task| {
                    task.executions.len() == 1
                        && task.executions.last().is_some_and(|execution| {
                            matches!(execution.review, Some(ReviewState::Rejected { .. }))
                        })
                })
        })
        .await?;
        assert_review_does_not_retry(&fixture, &id, &blocked, 1).await?;

        Ok(())
    }
    .await;
    fixture.finish(result).await
}
