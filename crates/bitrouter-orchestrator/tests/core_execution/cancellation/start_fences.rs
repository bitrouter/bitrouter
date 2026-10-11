//! Cancellation fences the durable start boundary before tool.cancel delivery.

use super::*;
use bitrouter_orchestrator::core::checkpoint::ToolStartFence;
use bitrouter_orchestrator::core::protocol::{ToolObservation, ToolStatus};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
enum CancelKind {
    Run,
    RuntimeSubtree,
    ModelSubtree,
    RootFailure,
}

impl CancelKind {
    fn event(self) -> &'static str {
        match self {
            Self::Run => "run.cancelling",
            Self::RuntimeSubtree => "collaboration.runtime",
            Self::ModelSubtree => "collaboration.applied",
            Self::RootFailure => "agent.failed",
        }
    }

    fn affected(self, command: &ToolExecute, root: &str, sibling: &str) -> bool {
        match self {
            Self::Run => true,
            Self::RuntimeSubtree | Self::ModelSubtree => {
                command.agent_id != root && command.agent_id != sibling
            }
            Self::RootFailure => command.agent_id != root,
        }
    }

    fn model_driven(self) -> bool {
        matches!(self, Self::ModelSubtree | Self::RootFailure)
    }
}

fn fence(command: &ToolExecute) -> ToolStartFence {
    ToolStartFence {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
    }
}

async fn cancellation_ack_loss(kind: CancelKind, persisted: bool) -> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.wait_for_approval = true;
    let harness = Arc::new(fixture);
    let port = Arc::new(reconnect::FaultPort::new(
        harness.clone(),
        kind.event(),
        persisted,
    ));
    port.set_enabled(false);
    let (session, executor, _) = setup(
        (0..16).map(|_| output(vec![text("completed")])).collect(),
        port.clone(),
        false,
    )
    .await?;
    let mut storage = signal_update(&session, Vec::new()).await;
    storage.manifest.artifact_quota_bytes = 32 * 1024 * 1024;
    session.signals("storage", storage).await?;
    let accepted = session
        .start("input", session.head().await.state_revision, input())
        .await?;
    let root = &accepted.assigned_ids["agent_id"];
    let child = spawn_child(&session, root, "child").await?;
    let grandchild = spawn_child(&session, &child, "grandchild").await?;
    let sibling = spawn_child(&session, root, "sibling").await?;
    for actor in [root, &child, &grandchild, &sibling] {
        executor
            .agent_once
            .lock()
            .await
            .insert(actor.clone(), vec![call(&format!("read-{actor}"))]);
    }
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 4);
    let running = commands
        .iter()
        .find(|command| command.agent_id == grandchild)
        .ok_or("running tool")?;
    assert!(harness.store.lock().await.try_start_tool(fence(running)));
    session
        .tool_status(
            "running",
            ToolObservation {
                invocation_id: running.invocation_id.clone(),
                attempt_id: running.attempt_id.clone(),
                status: ToolStatus::Running,
                evidence: Vec::new(),
            },
        )
        .await?;
    if kind.model_driven() {
        let root_tool = commands
            .iter()
            .find(|command| &command.agent_id == root)
            .ok_or("root tool")?;
        assert!(harness.store.lock().await.try_start_tool(fence(root_tool)));
        session
            .tool_result("root-result", result(root_tool))
            .await?;
        executor.agent_once.lock().await.insert(
            root.clone(),
            vec![if matches!(kind, CancelKind::ModelSubtree) {
                core_call("interrupt_agent", json!({"agent_id":child}), "interrupt")
            } else {
                core_call("unavailable_workspace_tool", json!({}), "invalid-output")
            }],
        );
    }
    // A rejected stale caller intent must not revoke any tool start.
    let stale = session
        .cancel_run("stale", 0, &accepted.assigned_ids["run_id"])
        .await
        .err()
        .ok_or("stale cancel accepted")?;
    assert_eq!(stale.code, ErrorCode::StaleRevision);
    assert!(harness.store.lock().await.tool_start_fences.is_empty());
    let revision = session.head().await.state_revision;
    port.set_enabled(true);
    let error = match kind {
        CancelKind::Run => session
            .cancel_run("cancel", revision, &accepted.assigned_ids["run_id"])
            .await
            .map(|_| ()),
        CancelKind::RuntimeSubtree => session
            .collaborate(
                "interrupt",
                revision,
                root,
                Action::Interrupt {
                    agent_id: child.clone(),
                },
            )
            .await
            .map(|_| ()),
        CancelKind::ModelSubtree | CancelKind::RootFailure => session.drive().await.map(|_| ()),
    }
    .err()
    .ok_or("missing cancellation ACK loss")?;
    assert_eq!(
        (error.code, error.commit_status),
        (ErrorCode::CheckpointUnavailable, CommitStatus::Unknown),
        "{kind:?}/{persisted}: {error:?}"
    );
    assert!(harness.cancelled.lock().await.is_empty());
    let proposal = port
        .proposals
        .lock()
        .await
        .last()
        .ok_or("cancel proposal")?
        .clone();
    let payload = proposal.decode(&Limits::default())?;
    assert!(
        payload
            .events
            .iter()
            .any(|event| event.kind == kind.event())
    );
    let expected = commands
        .iter()
        .filter(|command| kind.affected(command, root, &sibling))
        .map(fence)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        payload
            .tool_start_fences
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(payload.tool_start_fences.len(), expected.len());
    {
        let store = harness.store.lock().await;
        assert_eq!(
            store.head.state_revision,
            payload.base_state_revision + u64::from(persisted)
        );
        assert_eq!(
            store.tool_start_fences,
            if persisted {
                expected.clone()
            } else {
                BTreeSet::new()
            }
        );
        // Probe independent copies so testing admission cannot itself start a tool.
        for command in &commands {
            if command.invocation_id != running.invocation_id
                && !(kind.model_driven() && &command.agent_id == root)
            {
                let mut probe = store.clone();
                assert_eq!(
                    probe.try_start_tool(fence(command)),
                    !persisted || !kind.affected(command, root, &sibling)
                );
            }
        }
    }
    let blocked = session
        .drive()
        .await
        .err()
        .ok_or("pending ACK reopened driver")?;
    assert_eq!(blocked.commit_status, CommitStatus::Unknown);
    reconnect::reconnect(&session, &harness).await?;
    assert_eq!(harness.store.lock().await.tool_start_fences, expected);
    assert_eq!(
        harness
            .store
            .lock()
            .await
            .batches
            .iter()
            .filter(|batch| batch.identity.batch_id == proposal.identity.batch_id)
            .count(),
        1
    );
    let proposals = port.proposals.lock().await;
    let retries = proposals
        .iter()
        .filter(|batch| batch.identity.batch_id == proposal.identity.batch_id)
        .collect::<Vec<_>>();
    assert_eq!(retries.len(), if persisted { 1 } else { 2 });
    assert!(retries.iter().all(|batch| **batch == proposal));
    drop(proposals);
    let head = session.head().await;
    match kind {
        CancelKind::Run => {
            session
                .cancel_run("cancel", revision, &accepted.assigned_ids["run_id"])
                .await?;
        }
        CancelKind::RuntimeSubtree => {
            session
                .collaborate(
                    "interrupt",
                    revision,
                    root,
                    Action::Interrupt {
                        agent_id: child.clone(),
                    },
                )
                .await?;
        }
        CancelKind::ModelSubtree | CancelKind::RootFailure => {}
    }
    assert_eq!(session.head().await, head);
    let state = session.snapshot().await;
    for command in &commands {
        let invocation = state.agents[&command.agent_id]
            .turn
            .as_ref()
            .ok_or("turn")?
            .invocations
            .iter()
            .find(|call| call.dispatch.invocation_id == command.invocation_id)
            .ok_or("call")?;
        if !(kind.model_driven() && &command.agent_id == root) {
            assert!(
                invocation.result.is_none(),
                "fence must not invent a result"
            );
        }
    }
    // A committed fence wins over a late approval/execute, while a start that
    // already won remains a real running effect requiring its actual outcome.
    assert!(
        harness
            .store
            .lock()
            .await
            .started_tools
            .contains(&fence(running))
    );
    for command in &commands {
        if kind.model_driven() && &command.agent_id == root {
            continue;
        }
        let affected = kind.affected(command, root, &sibling);
        if command.invocation_id != running.invocation_id {
            assert_eq!(
                harness.store.lock().await.try_start_tool(fence(command)),
                !affected
            );
        }
    }
    session.drive().await?;
    for (index, command) in commands.iter().enumerate() {
        if kind.model_driven() && &command.agent_id == root {
            continue;
        }
        let mut value = result(command);
        if kind.affected(command, root, &sibling) && command.invocation_id != running.invocation_id
        {
            value.status = ToolOutcome::NotExecuted;
        }
        session
            .tool_result(&format!("result-{index}"), value)
            .await?;
    }
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().ok_or("run")?.status,
        match kind {
            CancelKind::Run => RunStatus::Cancelled,
            CancelKind::RootFailure => RunStatus::Failed,
            CancelKind::RuntimeSubtree | CancelKind::ModelSubtree => RunStatus::Completed,
        }
    );
    assert_eq!(harness.sent.lock().await.len(), 4);
    session
        .release("release", session.head().await.state_revision)
        .await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_start_fences_survive_ack_loss_and_preserve_subtree_scope() -> TestResult {
    for kind in [
        CancelKind::Run,
        CancelKind::RuntimeSubtree,
        CancelKind::ModelSubtree,
        CancelKind::RootFailure,
    ] {
        for persisted in [false, true] {
            cancellation_ack_loss(kind, persisted)
                .await
                .map_err(|error| format!("{kind:?}/{persisted}: {error}"))?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn restoring_legacy_cancellation_reasserts_fences_without_resolving_unknown_effects()
-> TestResult {
    for unknown in [false, true] {
        let mut fixture = Harness::new(None, None);
        fixture.wait_for_approval = true;
        let harness = Arc::new(fixture);
        let (session, _, _) =
            setup(vec![output(vec![call("pending")])], harness.clone(), false).await?;
        let accepted = session.start("input", 1, input()).await?;
        session.drive().await?;
        let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
        let uncertain = if unknown {
            let mut value = result(&command);
            value.status = ToolOutcome::EffectUnknown;
            session.tool_result("uncertain", value.clone()).await?;
            Some(value)
        } else {
            None
        };
        session
            .cancel_run(
                "cancel",
                session.head().await.state_revision,
                &accepted.assigned_ids["run_id"],
            )
            .await?;
        session.disconnect().await;
        let mut legacy = harness.store.lock().await.clone();
        assert!(legacy.tool_start_fences.contains(&fence(&command)));
        // Recreate the historical producer's unfenced cancellation checkpoint.
        // This does not model a conforming harness dropping durable tombstones.
        let original = legacy.batches.pop().ok_or("cancellation batch")?;
        let mut payload = original.decode(&legacy.limits)?;
        assert!(
            payload
                .events
                .iter()
                .any(|event| event.kind == "run.cancelling")
        );
        payload.tool_start_fences.clear();
        legacy.acknowledgements.remove(&original.identity.batch_id);
        let previous = legacy.batches.last().ok_or("previous batch")?;
        legacy.head = legacy.acknowledgements[&previous.identity.batch_id].head();
        legacy.tool_start_fences.clear();
        legacy.commit(&CheckpointBatch::encode(&payload, &legacy.limits)?)?;
        let replacement = recovery::harness_at(legacy).await;
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        request.tools.push(ToolObservation {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
            status: if unknown {
                ToolStatus::EffectUnknown
            } else {
                ToolStatus::WaitingApproval
            },
            evidence: Vec::new(),
        });
        let (restored, executor) =
            recovery::restore(request, replacement.clone(), Vec::new()).await?;
        let store = replacement.store.lock().await;
        let last = store
            .batches
            .last()
            .ok_or("restore checkpoint")?
            .decode(&store.limits)?;
        assert!(
            last.events
                .iter()
                .any(|event| event.kind == "session.restored")
        );
        assert_eq!(last.tool_start_fences, vec![fence(&command)]);
        assert!(store.tool_start_fences.contains(&fence(&command)));
        let mut probe = store.clone();
        assert!(!probe.try_start_tool(fence(&command)));
        drop(store);
        let pending = restored.drive().await?;
        let call = &pending.root_turn().ok_or("turn")?.invocations[0];
        assert_eq!(call.result, uncertain);
        assert!(!call.consumed);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(replacement.sent.lock().await.is_empty());
        let mut definite = result(&command);
        definite.status = ToolOutcome::NotExecuted;
        if unknown {
            assert_eq!(
                pending.run.as_ref().ok_or("run")?.status,
                RunStatus::RecoveryRequired
            );
            assert!(
                restored
                    .tool_result("untrusted-reconciliation", definite.clone())
                    .await
                    .is_err()
            );
            restored.disconnect().await;
            let reconciler = recovery::harness_at(replacement.store.lock().await.clone()).await;
            let mut request = recovery::request(&*reconciler.store.lock().await, false)?;
            request.results.push(definite.clone());
            let (reconciled, executor) = recovery::restore(request, reconciler, Vec::new()).await?;
            let done = reconciled.drive().await?;
            assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Cancelled);
            let call = &done.root_turn().ok_or("turn")?.invocations[0];
            assert_eq!(call.prior_uncertain_result, uncertain);
            assert_eq!(call.result.as_ref(), Some(&definite));
            assert!(call.consumed);
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
            reconciled
                .release("release", reconciled.head().await.state_revision)
                .await?;
        } else {
            assert_eq!(
                pending.run.as_ref().ok_or("run")?.status,
                RunStatus::Cancelling
            );
            restored.tool_result("not-executed", definite).await?;
            assert_eq!(
                restored.drive().await?.run.as_ref().ok_or("run")?.status,
                RunStatus::Cancelled
            );
            restored
                .release("release", restored.head().await.state_revision)
                .await?;
        }
    }
    Ok(())
}
