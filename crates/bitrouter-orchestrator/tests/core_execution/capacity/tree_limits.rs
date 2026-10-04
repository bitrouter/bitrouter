//! Exercise the default graph, concurrency and outstanding-tool bounds together.

use super::*;
use bitrouter_orchestrator::core::protocol::OperationDisposition;
use bitrouter_orchestrator::core::session::AgentStatus;

struct BoundedExecutor {
    inner: RecordingExecutor,
    seen: Semaphore,
    release: Semaphore,
    active: AtomicUsize,
    peak: AtomicUsize,
    entered: AtomicUsize,
    burst_agent: Mutex<Option<String>>,
    burst_seen: Semaphore,
    burst_release: Semaphore,
}

#[async_trait]
impl Executor for BoundedExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        if self.entered.fetch_add(1, Ordering::SeqCst) < 4 {
            self.seen.add_permits(1);
            self.release
                .acquire()
                .await
                .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?
                .forget();
        }
        let burst = self.burst_agent.lock().await.clone();
        if burst.as_ref().is_some_and(|id| {
            prompt.system.as_ref().is_some_and(|system| {
                system.starts_with(&format!("You are agent {id} for this task."))
            })
        }) {
            self.burst_agent.lock().await.take();
            self.burst_seen.add_permits(1);
            self.burst_release
                .acquire()
                .await
                .map_err(|error| bitrouter_sdk::BitrouterError::internal(error.to_string()))?
                .forget();
        }
        let result = self.inner.execute(target, prompt, ctx).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }

    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        Err(bitrouter_sdk::BitrouterError::internal(
            "unexpected streaming",
        ))
    }
}

async fn rejected_spawn(session: &CoreSession, actor: &str, operation: &str) -> TestResult {
    let before = session.snapshot().await;
    let head = session.head().await;
    let action = Action::Spawn {
        task: work("must not be dispatched"),
    };
    let error = session
        .collaborate(operation, head.state_revision, actor, action.clone())
        .await
        .err()
        .ok_or("oversized tree accepted")?;
    assert_eq!(error.code, ErrorCode::LimitExceeded);
    assert_eq!(
        serde_json::to_value(session.snapshot().await.agents)?,
        serde_json::to_value(before.agents)?
    );
    let receipt = session.operation(operation).await.ok_or("rejection")?;
    assert_eq!(receipt.disposition, OperationDisposition::Rejected);
    let rejected = session.head().await;
    assert_eq!(rejected.state_revision, head.state_revision + 1);
    let replay = session
        .collaborate(operation, head.state_revision, actor, action)
        .await
        .err()
        .ok_or("rejected operation became accepted")?;
    assert_eq!(serde_json::to_value(replay)?, serde_json::to_value(error)?);
    assert_eq!(session.operation(operation).await, Some(receipt));
    assert_eq!(session.head().await, rejected);
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Case {
    MaximumTree,
    MaximumTools,
    CheckpointCapacity,
    ArtifactCapacity,
}

async fn saturate_observations(
    session: &CoreSession,
    harness: &Harness,
    command: &ToolExecute,
) -> TestResult {
    for index in 0..32 {
        let observation = full_observation(
            harness,
            command,
            ToolStatus::WaitingApproval,
            &format!("pressure-{index}"),
        )
        .await?;
        match session
            .tool_status(&format!("pressure-{index}"), observation)
            .await
        {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(
                    (error.code, error.commit_status),
                    (ErrorCode::LimitExceeded, CommitStatus::NotCommitted)
                );
                let state = session.snapshot().await;
                let run = state.run.as_ref().ok_or("run")?;
                assert_eq!(
                    run.resource_constraint,
                    Some(ResourceConstraint::CheckpointCapacity)
                );
                assert!(run.active_ms < run.limits.active_seconds * 1000);
                return Ok(());
            }
        }
    }
    Err("full observations did not reach byte admission".into())
}

async fn maximum_tree(case: Case, committed: bool) -> TestResult {
    let (agent_count, tool_count, quota, failed) = match case {
        Case::MaximumTree => (32, 1, 256 * 1024 * 1024, false),
        Case::MaximumTools => (6, 8, 256 * 1024 * 1024, false),
        Case::CheckpointCapacity => (32, 8, 256 * 1024 * 1024, true),
        Case::ArtifactCapacity => (6, 8, 4 * 1024 * 1024, true),
    };
    let expected_commands = match case {
        Case::MaximumTree => 1,
        Case::MaximumTools | Case::CheckpointCapacity => 8,
        Case::ArtifactCapacity => 0,
    };
    let terminal = if failed {
        "run.failed"
    } else {
        "run.completed"
    };
    let harness = Arc::new(Harness::new(None, None));
    let port = Arc::new(reconnect::FaultPort::new(
        harness.clone(),
        terminal,
        committed,
    ));
    let executor = Arc::new(BoundedExecutor {
        inner: RecordingExecutor {
            // Parents may model again after consuming child conclusions.
            // MockExecutor pops replies, including the always_text constructor.
            mock: MockExecutor::new(
                (0..Limits::default().model_attempts)
                    .map(|_| output(vec![text("owned work completed")]))
                    .collect(),
            ),
            agent_once: Mutex::new(Default::default()),
            prompts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        },
        seen: Semaphore::new(0),
        release: Semaphore::new(0),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        entered: AtomicUsize::new(0),
        burst_agent: Mutex::new(None),
        burst_seen: Semaphore::new(0),
        burst_release: Semaphore::new(0),
    });
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let settlements = Arc::new(AtomicUsize::new(0));
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone())
                .settlement_recorder(Recorder(settlements.clone()));
        })
        .build()?;
    let session = bind_app(Arc::new(app), port.clone()).await?;
    // The common fixture advertises only 4 MiB of artifact storage. Eight
    // complete reply/archive contracts need their own adequate harness quota;
    // this does not enlarge any execution, checkpoint or JSON payload limit.
    let mut storage = signal_update(&session, Vec::new()).await;
    storage.manifest.artifact_quota_bytes = quota;
    session.signals("storage", storage).await?;
    let root = session
        .start("input", session.head().await.state_revision, input())
        .await?
        .assigned_ids["agent_id"]
        .clone();
    let mut agents = vec![root.clone()];
    for depth in 1..=4 {
        let parent = agents.last().ok_or("parent")?;
        let receipt = session
            .collaborate(
                &format!("depth-{depth}"),
                session.head().await.state_revision,
                parent,
                Action::Spawn {
                    task: work(&format!("depth-{depth}")),
                },
            )
            .await?;
        agents.push(receipt.assigned_ids["agent_id"].clone());
    }
    rejected_spawn(&session, &agents[4], "depth-overflow").await?;
    for index in 5..agent_count {
        let receipt = session
            .collaborate(
                &format!("width-{index}"),
                session.head().await.state_revision,
                &root,
                Action::Spawn {
                    task: work(&format!("width-{index}")),
                },
            )
            .await?;
        agents.push(receipt.assigned_ids["agent_id"].clone());
    }
    if agent_count == 32 {
        rejected_spawn(&session, &root, "width-overflow").await?;
    }
    let state = session.snapshot().await;
    assert_eq!(state.agents.len(), agent_count);
    assert_eq!(
        state.agents.values().map(|agent| agent.depth).max(),
        Some(4)
    );
    assert_eq!(state.run.as_ref().ok_or("run")?.limits, Limits::default());
    // Counts and byte budgets are independent ceilings. Hold the tool-bearing
    // model until all other branches finish and the root has consumed their
    // conclusions, isolating the tool contract from later join-model reports.
    // Pick the earliest scheduled root leaf so this barrier does not wait for
    // a random UUID's position behind the rest of the large tree.
    let burst = agents[5..].iter().min().ok_or("burst agent")?.clone();
    *executor.burst_agent.lock().await = Some(burst.clone());
    for agent in &agents {
        let content = if *agent == burst {
            (0..tool_count)
                .map(|index| call(&format!("leaf-{index}")))
                .collect()
        } else {
            vec![text(&format!("owned work completed by {agent}"))]
        };
        executor
            .inner
            .agent_once
            .lock()
            .await
            .insert(agent.clone(), content);
    }
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(30), executor.seen.acquire_many(4))
        .await
        .map_err(|_| "initial four models did not reach the executor barrier")??
        .forget();
    assert_eq!(executor.active.load(Ordering::SeqCst), 4);
    assert_eq!(executor.peak.load(Ordering::SeqCst), 4);
    assert_eq!(executor.entered.load(Ordering::SeqCst), 4);
    executor.release.add_permits(4);
    tokio::time::timeout(Duration::from_secs(120), executor.burst_seen.acquire())
        .await
        .map_err(|_| "tool-bearing model did not reach the executor barrier")??
        .forget();
    let quiet = tokio::time::timeout(Duration::from_secs(240), async {
        let mut progress_at = std::time::Instant::now();
        loop {
            let state = session.snapshot().await;
            if progress_at.elapsed() >= Duration::from_secs(10) {
                let head = session.head().await;
                eprintln!(
                    "{case:?}/{committed}: waiting for tree joins; revision={} event={} entered={} active={}",
                    head.state_revision,
                    head.event_seq,
                    executor.entered.load(Ordering::SeqCst),
                    executor.active.load(Ordering::SeqCst)
                );
                progress_at = std::time::Instant::now();
            }
            if state
                .agents
                .values()
                .filter(|agent| agent.agent_id != burst)
                .all(|agent| {
                    agent.turn.as_ref().is_some_and(|turn| {
                        !turn.steps.is_empty()
                            && turn.steps.iter().all(|step| step.settled)
                            && if agent.agent_id == root {
                                turn.final_answer.is_some()
                                    && agent.mailbox.iter().all(|mail| mail.consumed)
                            } else {
                                turn.status == AgentStatus::Completed && turn.notified
                            }
                    })
                })
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if quiet.is_err() {
        let state = session.snapshot().await;
        eprintln!(
            "barrier run={:?}; driver finished={}",
            state.run,
            running.is_finished()
        );
        for agent in state.agents.values() {
            if let Some(turn) = &agent.turn {
                eprintln!(
                    "agent={} depth={} burst={} status={:?} notified={} answer={} settled={:?} mail={:?}",
                    agent.agent_id,
                    agent.depth,
                    agent.agent_id == burst,
                    turn.status,
                    turn.notified,
                    turn.final_answer.is_some(),
                    turn.steps
                        .iter()
                        .map(|step| step.settled)
                        .collect::<Vec<_>>(),
                    agent
                        .mailbox
                        .iter()
                        .map(|mail| mail.consumed)
                        .collect::<Vec<_>>()
                );
            }
        }
    }
    quiet?;
    let retained_bytes = serde_json::to_vec(&session.snapshot().await)?.len();
    eprintln!("{case:?}/{committed}: pre-tool snapshot bytes={retained_bytes}");
    executor.burst_release.add_permits(1);
    let initial = tokio::time::timeout(Duration::from_secs(120), running)
        .await
        .map_err(|_| "driver did not settle after releasing the tool model")??;
    let mut outcomes = Vec::new();
    let (commands, error) = if expected_commands == 0 {
        assert!(harness.sent.lock().await.is_empty());
        (
            Vec::new(),
            initial.err().ok_or("capacity terminal ACK loss missing")?,
        )
    } else {
        let waiting = initial?;
        assert_eq!(
            waiting.run.as_ref().ok_or("run")?.status,
            RunStatus::Waiting,
            "run={:?}; models={}; tools={}",
            waiting.run,
            executor.inner.calls.load(Ordering::SeqCst),
            harness.sent.lock().await.len()
        );
        let commands = harness.sent.lock().await.clone();
        assert_eq!(commands.len(), expected_commands);
        assert!(commands.iter().all(|command| command.agent_id == burst));
        if matches!(case, Case::CheckpointCapacity) {
            saturate_observations(&session, &harness, commands.first().ok_or("tool")?).await?;
        }
        for (index, command) in commands.iter().enumerate() {
            let outcome = full_result(
                &harness,
                command,
                ToolOutcome::Succeeded,
                &format!("leaf-evidence-{index}"),
            )
            .await?;
            session
                .tool_result(&format!("result-{index}"), outcome.clone())
                .await?;
            outcomes.push(outcome);
        }
        let error = tokio::time::timeout(Duration::from_secs(120), session.drive())
            .await
            .map_err(|_| "driver did not settle after recording all tool results")?
            .err()
            .ok_or("terminal ACK loss missing")?;
        (commands, error)
    };
    assert_eq!(error.commit_status, CommitStatus::Unknown);
    let proposal = port
        .proposals
        .lock()
        .await
        .last()
        .ok_or("proposal")?
        .clone();
    assert_eq!(
        proposal.decode(&Limits::default())?.events[0].kind,
        terminal
    );
    let calls = executor.inner.calls.load(Ordering::SeqCst);
    reconnect::reconnect(&session, &harness).await?;
    let done = session.drive().await?;
    let run = done.run.as_ref().ok_or("run")?;
    assert_eq!(
        run.status,
        if failed {
            RunStatus::Failed
        } else {
            RunStatus::Completed
        }
    );
    if failed {
        assert_eq!(
            run.resource_constraint,
            Some(ResourceConstraint::CheckpointCapacity)
        );
        assert_eq!(
            run.resource_error.as_ref().ok_or("resource error")?.code,
            ErrorCode::LimitExceeded
        );
        assert!(run.active_ms < run.limits.active_seconds * 1000);
        assert!(done.root_queue.paused);
    } else {
        assert!(run.resource_error.is_none());
    }
    assert_eq!(done.agents.len(), agent_count);
    assert_eq!(
        done.agents
            .values()
            .filter_map(|agent| agent.turn.as_ref())
            .map(|turn| turn.invocations.len())
            .sum::<usize>(),
        expected_commands
    );
    assert!(done.agents.values().all(|agent| {
        agent.queue.is_empty()
            && agent.turn.as_ref().is_some_and(|turn| {
                turn.status == AgentStatus::Completed
                    || (failed
                        && matches!(
                            turn.status,
                            AgentStatus::Interrupted | AgentStatus::Failed | AgentStatus::Cancelled
                        ))
            })
    }));
    for (command, outcome) in commands.iter().zip(&outcomes) {
        let agent = &done.agents[&command.agent_id];
        let turn = agent.turn.as_ref().ok_or("turn")?;
        let call = turn
            .invocations
            .iter()
            .find(|call| call.dispatch.invocation_id == command.invocation_id)
            .ok_or("tool call")?;
        assert_eq!(call.result.as_ref(), Some(outcome));
        assert!(call.consumed);
        let paired = agent.history.iter().flat_map(|message| &message.content).filter(|part| {
            matches!(part, Content::ToolResult { call_id, tool_name, output: bitrouter_sdk::language_model::types::ToolResultOutput::Text { value }, .. } if call_id == &call.provider_call_id && tool_name.as_deref() == Some(command.tool.as_str()) && value == &outcome.output)
        }).count();
        assert_eq!(paired, 1);
    }
    for child_id in &agents[1..] {
        let child = &done.agents[child_id];
        let turn = child.turn.as_ref().ok_or("turn")?;
        assert!(
            turn.status == AgentStatus::Completed
                || (failed
                    && matches!(
                        turn.status,
                        AgentStatus::Interrupted | AgentStatus::Failed | AgentStatus::Cancelled
                    ))
        );
        let notices = done.agents[&turn.assigned_by]
            .mailbox
            .iter()
            .filter(|mail| mail.kind == "agent_result" && mail.sender_id == *child_id)
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].context_sources, child.context_sources);
        assert_eq!(notices[0].content["agent_id"], *child_id);
        assert_eq!(notices[0].content["agent_turn_id"], turn.agent_turn_id);
        assert_eq!(notices[0].content["answer"], json!(turn.final_answer));
    }
    let prompts = executor.inner.prompts.lock().await;
    for agent in &agents {
        let actual = prompts
            .iter()
            .filter(|prompt| {
                prompt.system.as_ref().is_some_and(|system| {
                    system.starts_with(&format!("You are agent {agent} for this task."))
                })
            })
            .count();
        assert!(actual > 0, "agent {agent} had no model execution");
        if *agent != burst {
            let answer = format!("owned work completed by {agent}");
            assert!(
                done.agents[agent]
                    .history
                    .iter()
                    .filter(|message| message.role
                        == bitrouter_sdk::language_model::types::Role::Assistant)
                    .flat_map(|message| &message.content)
                    .any(|part| matches!(part, Content::Text { text, .. } if text == &answer)),
                "answer attribution missing for {agent}"
            );
        }
        assert_eq!(
            actual,
            done.agents[agent].turn.as_ref().ok_or("turn")?.steps.len()
        );
    }
    drop(prompts);
    assert_eq!(executor.inner.calls.load(Ordering::SeqCst), calls);
    assert_eq!(settlements.load(Ordering::SeqCst), calls);
    assert_eq!(executor.peak.load(Ordering::SeqCst), 4);
    assert_eq!(executor.active.load(Ordering::SeqCst), 0);
    assert_eq!(harness.sent.lock().await.len(), expected_commands);
    assert_eq!(
        port.proposals
            .lock()
            .await
            .iter()
            .filter(|batch| **batch == proposal)
            .count(),
        if committed { 1 } else { 2 }
    );
    session
        .release("release", session.head().await.state_revision)
        .await?;
    for batch in &harness.store.lock().await.batches {
        assert!(batch.wire_bytes()? <= Limits::default().unacknowledged_bytes);
        assert!(
            serde_json::to_vec(&batch.decode(&Limits::default())?)?.len() as u64
                <= Limits::default().checkpoint_bytes
        );
    }
    Ok(())
}

#[tokio::test]
async fn maximum_tree_completes_with_default_counts_and_terminal_ack_loss() -> TestResult {
    maximum_tree(Case::MaximumTree, false).await
}

#[tokio::test]
async fn maximum_tool_batch_keeps_full_replies_after_terminal_ack_loss() -> TestResult {
    maximum_tree(Case::MaximumTools, true).await
}

#[tokio::test]
async fn maximum_tree_byte_failure_survives_terminal_ack_loss_before_persistence() -> TestResult {
    maximum_tree(Case::CheckpointCapacity, false).await
}

#[tokio::test]
async fn maximum_tree_byte_failure_survives_terminal_ack_loss_after_persistence() -> TestResult {
    maximum_tree(Case::CheckpointCapacity, true).await
}

#[tokio::test]
async fn insufficient_artifact_quota_never_dispatches_the_tool_batch() -> TestResult {
    maximum_tree(Case::ArtifactCapacity, true).await
}
