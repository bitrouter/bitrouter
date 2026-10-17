//! Actual ACP processes must be owned by shared Turns, not another lease owner.
use super::*;
use bitrouter_orchestrator::thread::{ThreadTarget, executor::Config};

pub(super) async fn assert_shared(
    fixture: &Fixture,
    goal: &GoalSnapshot,
    complete: bool,
) -> Result<()> {
    for task in &goal.workflow.tasks {
        let Some(execution) = task.executions.last() else {
            continue;
        };
        let link = execution
            .thread
            .as_ref()
            .context("ACP execution has no common Thread/Turn")?;
        let target = ThreadTarget {
            thread_id: link.thread_id.clone(),
            server_instance_id: fixture.threads.capabilities().server_instance_id,
        };
        let caller = CallerContext::local();
        let view = fixture
            .threads
            .read_stored_thread_view(&target, &caller)
            .await?;
        let Config::Acp { acp } = &view.config else {
            anyhow::bail!("ACP worker was admitted as a native Agent");
        };
        assert_eq!(
            Some(acp.agent_id.as_str()),
            task.execution_harness(execution)
        );
        assert!(view.config.native().is_err());
        assert_eq!(
            view.thread.goal_work.as_ref(),
            Some(&super::super::native::source(goal, task, execution)?)
        );
        let turn = fixture
            .threads
            .read_stored_turn(&target, &caller, &link.turn_id)
            .await?;
        let binding = turn
            .acp
            .as_ref()
            .context("actual ACP session binding missing")?;
        let owner = bitrouter_orchestrator::service::worker::acp::Owner {
            server_instance_id: target.server_instance_id,
            thread_id: link.thread_id.clone(),
            turn_id: link.turn_id.clone(),
        };
        let (snapshot, replay) = fixture
            .supervisor
            .managed_observation(&owner, &binding.run_id, 0)
            .await?;
        assert_eq!(snapshot.managed_by.as_ref(), Some(&owner));
        assert_eq!(
            snapshot.native_session_id.as_ref(),
            Some(&binding.session_id)
        );
        assert!(snapshot.lease.is_none());
        assert!(replay.history_complete);
        if complete {
            assert_eq!(turn.status, TurnStatus::Completed, "{:?}", turn.detail);
            assert!(!turn.unknown_effect);
            assert!(turn.acp_stop.as_ref().is_some_and(
                |stop| stop.confirmed && stop.run_id.as_ref() == Some(&binding.run_id)
            ));
            assert!(snapshot.owned_process_terminated());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generic_supervisor_control_cannot_take_over_a_managed_turn() -> Result<()> {
    let fixture = Setup::new().await?.start().await?;
    let result = async {
        let id = fixture.create().await?;
        let running = fixture.running(&id).await?;
        assert_shared(&fixture, &running, false).await?;
        let SessionResponse::Authorized { grant } = fixture
            .supervisor
            .dispatch(SessionRequest::authorize(
                "other-supervisor-client".into(),
                BTreeSet::from([SessionScope::Respond, SessionScope::Stop]),
            ))
            .await?
        else {
            anyhow::bail!("control grant missing");
        };
        for task in &running.workflow.tasks {
            let Some(execution) = task.executions.last() else {
                continue;
            };
            let Observation::Running {
                run_id: Some(run_id),
                ..
            } = &execution.observation
            else {
                continue;
            };
            let response = fixture
                .supervisor
                .dispatch(SessionRequest::authorized(
                    &grant,
                    SessionCommand::AcquireLease {
                        run_id: run_id.clone(),
                        client_id: grant.client_id.clone(),
                        mode: LeaseMode::Transient,
                        action_request_id: format!("takeover-{}", execution.id),
                        takeover: true,
                    },
                ))
                .await;
            assert!(
                response
                    .err()
                    .is_some_and(|error| error.to_string().contains("Thread/Turn"))
            );
            let response = fixture
                .supervisor
                .dispatch(SessionRequest::authorized(
                    &grant,
                    SessionCommand::Stop {
                        fence: ControlFence {
                            run_id: run_id.clone(),
                            client_id: grant.client_id.clone(),
                            lease_generation: 0,
                            action_request_id: format!("outside-stop-{}", execution.id),
                        },
                        confirmed: true,
                    },
                ))
                .await;
            assert!(
                response
                    .err()
                    .is_some_and(|error| error.to_string().contains("Thread/Turn"))
            );
        }
        assert_shared(&fixture, &running, false).await?;
        fixture.release().await?;
        let completed = fixture.completed(&id, 1).await?;
        assert_shared(&fixture, &completed, true).await?;
        Ok(())
    }
    .await;
    fixture.finish(result).await
}
