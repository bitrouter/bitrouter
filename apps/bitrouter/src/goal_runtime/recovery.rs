//! Replacements require scoped lifecycle/tool evidence and a frozen workspace.
use super::*;
use crate::supervisor::{ReplayBatch, SessionEventKind};
use bitrouter_orchestrator::goal::decision::{Event as PermissionEvent, PermissionDecision};
use bitrouter_orchestrator::goal::workflow::LocalReconciliation;

mod native;

impl Coordinator {
    /// Read committed observations/results at one cutoff; the live head prunes closed cards.
    async fn permission_receipts(
        &self,
        goal: &GoalSnapshot,
        execution: &Execution,
    ) -> Result<Vec<(PermissionDecision, Option<String>)>> {
        let mut after = 0;
        let mut requests = BTreeMap::<String, (PermissionDecision, Option<String>)>::new();
        loop {
            let response = self
                .goals
                .execute(
                    "local",
                    GoalCommand::History {
                        goal_id: goal.id.clone(),
                        after,
                        cutoff: Some(goal.revision),
                        limit: bitrouter_orchestrator::goal::MAX_PAGE_ITEMS,
                    },
                    now()?,
                )
                .await?;
            let GoalReply::History { page } = response else {
                anyhow::bail!("permission history is unavailable");
            };
            anyhow::ensure!(
                page.cutoff == goal.revision,
                "permission history cutoff changed"
            );
            for operation in page.operations {
                let bitrouter_orchestrator::goal::OperationKind::Workflow { change } =
                    operation.kind
                else {
                    continue;
                };
                match *change {
                    Change::Decision {
                        event: PermissionEvent::Observe { decision },
                    } if decision.execution_id == execution.id => {
                        requests.insert(decision.id.clone(), (decision, None));
                    }
                    Change::Decision {
                        event:
                            PermissionEvent::Result {
                                decision_id,
                                operation_id,
                                submitted,
                                ..
                            },
                    } => {
                        if let Some((_, delivered)) = requests.get_mut(&decision_id) {
                            *delivered = submitted.then_some(operation_id);
                        }
                    }
                    _ => {}
                }
            }
            let Some(next) = page.next_after else {
                break;
            };
            anyhow::ensure!(next > after, "permission history did not advance");
            after = next;
        }
        Ok(requests.into_values().collect())
    }

    pub(super) async fn reconcile_local_execution(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<()> {
        let control_source_revision = goal
            .control_source_revision
            .max(task.control_source_revision);
        if execution
            .reconciliation
            .as_ref()
            .is_some_and(|state| match state {
                LocalReconciliation::Verified {
                    task_revision,
                    control_source_revision: observed,
                    ..
                }
                | LocalReconciliation::Blocked {
                    task_revision,
                    control_source_revision: observed,
                    ..
                } => {
                    *task_revision == task.requirements_revision
                        && *observed == control_source_revision
                }
            })
        {
            return Ok(());
        }
        let result = self.local_recovery_evidence(goal, task, execution).await;
        let reconciliation = match result {
            Ok((workspace, transcript)) => LocalReconciliation::Verified {
                task_revision: task.requirements_revision,
                control_source_revision,
                workspace,
                transcript,
            },
            Err(error) => LocalReconciliation::Blocked {
                task_revision: task.requirements_revision,
                control_source_revision,
                reason: format!(
                    "No automatic replacement: {}. Reconcile the missing tool/workspace/external-effect evidence before retrying.",
                    bounded_error(&error)
                ),
            },
        };
        self.change(
            goal,
            Change::ReconcileStoppedExecution {
                execution_id: execution.id.clone(),
                reconciliation,
            },
        )
        .await
    }

    pub(super) async fn local_recovery_evidence(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<(ArtifactRef, ArtifactRef)> {
        anyhow::ensure!(
            matches!(execution.stop, Some(StopState::Confirmed { .. })),
            "execution stop is unconfirmed"
        );
        anyhow::ensure!(
            !matches!(execution.review, Some(ReviewState::Requested { .. }))
                && !execution.checks.iter().any(|check| matches!(
                    check.state,
                    bitrouter_orchestrator::goal::workflow::CheckState::Requested
                        | bitrouter_orchestrator::goal::workflow::CheckState::Observed {
                            completed: false,
                            ..
                        }
                )),
            "BRO check or semantic review is still unresolved"
        );
        if task.spec.harness.is_none() {
            return self
                .native_local_recovery_evidence(goal, task, execution)
                .await;
        }
        let run_id = match &execution.observation {
            Observation::Running {
                run_id: Some(run_id),
                ..
            }
            | Observation::Candidate {
                run_id: Some(run_id),
                ..
            } => run_id,
            Observation::Unknown {
                run_id: Some(run_id),
                ..
            }
            | Observation::Failed {
                run_id: Some(run_id),
                ..
            } => run_id,
            _ => anyhow::bail!("the original dispatch has no actual run mapping"),
        };
        let snapshot = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Snapshot {
                    run_id: run_id.clone(),
                },
            ))
            .await?;
        let SessionResponse::Snapshot { snapshot } = snapshot else {
            anyhow::bail!("stop snapshot is unavailable");
        };
        anyhow::ensure!(
            snapshot.run_id == *run_id
                && snapshot.owned_process_terminated()
                && snapshot.pending_permissions.is_empty(),
            "owned process is not stopped or has unresolved permissions"
        );
        anyhow::ensure!(
            tokio::fs::canonicalize(&snapshot.cwd).await?
                == tokio::fs::canonicalize(&execution.workspace).await?,
            "run workspace does not match the committed execution"
        );
        let replay = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Events {
                    run_id: run_id.clone(),
                    after_seq: 0,
                },
            ))
            .await?;
        let SessionResponse::Events { replay } = replay else {
            anyhow::bail!("original tool history is unavailable");
        };
        anyhow::ensure!(
            replay.history_complete
                && replay.first_retained_seq == 1
                && replay.snapshot_seq == snapshot.last_seq,
            "original tool history is incomplete or changed"
        );
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let mut approved = BTreeSet::new();
        let receipts = self.permission_receipts(goal, execution).await?;
        let mut submissions = Vec::new();
        let mut controller_denials = Vec::new();
        for event in &replay.events {
            let SessionEventKind::Permission { permission } = &event.kind else {
                continue;
            };
            let actual_request = serde_json::to_vec(permission)?;
            if let Some(resolution) = controller_denial(&replay, permission)? {
                let material =
                    crate::goal_permission_material::capture(permission, run_id, &replay)?;
                let tool = crate::goal_permission_material::denied_tool_scope(&material, run_id)?
                    .context("denied tool declarations are missing or contradictory")?;
                anyhow::ensure!(
                    permissions::is_local_tool(&tool, &execution.workspace).await?,
                    "denied operation had unsupported, external or unscoped effects"
                );
                controller_denials.push(serde_json::json!({"permission_id":permission.permission_id,
                    "tool_call_id":permission.tool_call.tool_call_id.0.as_ref(),
                    "resolution":resolution,"request":serde_json::from_slice::<serde_json::Value>(&material)?}));
                continue;
            }
            let mut matching = None;
            for (decision, delivery) in &receipts {
                if decision.run_id.as_ref() == Some(run_id)
                    && decision.permission_id == permission.permission_id
                    && let Some(delivery) = delivery
                    && crate::goal_permission_material::original_bytes(
                        &crate::goal_artifacts::read(&root, &decision.request).await?,
                    )? == actual_request
                {
                    matching = Some((decision, delivery));
                    break;
                }
            }
            let (decision, delivery) =
                matching.context("tool history has no matching durable permission submission")?;
            let option_id = replay
                .events
                .iter()
                .rev()
                .find_map(|event| match &event.kind {
                    SessionEventKind::PermissionResolved {
                        permission_id,
                        option_id,
                        ..
                    } if permission_id == &permission.permission_id => option_id.as_ref(),
                    _ => None,
                })
                .context("actual permission response is unresolved")?;
            let local =
                permissions::allowed_local_option(&root, decision, &execution.workspace).await?;
            anyhow::ensure!(
                local.as_ref() == Some(option_id),
                "permission included unsupported, persistent, external or unscoped effects"
            );
            let bytes = crate::goal_artifacts::read(&root, &decision.request).await?;
            let request: serde_json::Value = serde_json::from_slice(&bytes)?;
            approved.insert(
                request["tool_call"]["toolCallId"]
                    .as_str()
                    .context("permission omitted its actual tool identity")?
                    .to_owned(),
            );
            submissions.push(serde_json::json!({"decision_id":decision.id,"operation_id":delivery,
                "permission_id":permission.permission_id,"actual_option_id":option_id,"request_sha256":decision.request.sha256}));
        }
        let startup_abort = snapshot.confirmed_startup_abort();
        let mut failed_local_tools_without_requests = BTreeSet::new();
        if startup_abort {
            unopened_startup_history(&replay)?;
        } else {
            for tool in crate::goal_permission_material::failed_without_requests(&replay)? {
                anyhow::ensure!(
                    permissions::is_local_tool(&tool, &execution.workspace).await?,
                    "unrequested failed tool effects are unscoped or external"
                );
                let id = tool["toolCallId"]
                    .as_str()
                    .context("failed tool identity missing")?;
                failed_local_tools_without_requests.insert(id.to_owned());
            }
            completed_local_tools(&replay, &approved, &failed_local_tools_without_requests)?;
        }
        let base = execution
            .base_commit
            .as_deref()
            .context("workspace has no committed baseline")?;
        let workspace = crate::goal_artifacts::capture(&root, &execution.workspace, base).await?;
        let repeated = crate::goal_artifacts::capture(&root, &execution.workspace, base).await?;
        anyhow::ensure!(workspace == repeated, "stopped workspace is still changing");
        let again = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Snapshot {
                    run_id: run_id.clone(),
                },
            ))
            .await?;
        let SessionResponse::Snapshot { snapshot: again } = again else {
            anyhow::bail!("final stop snapshot is unavailable");
        };
        anyhow::ensure!(
            again.owned_process_terminated()
                && again.process == snapshot.process
                && again.startup_aborted_before_session == snapshot.startup_aborted_before_session
                && again.last_seq == replay.snapshot_seq
                && again.pending_permissions.is_empty(),
            "process or tool state changed during reconciliation"
        );
        let scope = if startup_abort {
            "controller failed before any native session request or Goal prompt; owned process teardown confirmed; no claim about ambient harness initialization effects or fees"
        } else {
            "finished declared read/edit/search inside the owned workspace only; offered permissions require actual resolutions. Calls without an offered request require failed terminal evidence and explicit local scope. Possible partial local effects are included in the stable workspace; failure is not proof of no effects or user consent."
        };
        let evidence = serde_json::json!({"scope":scope,
            "execution_id":execution.id,"run_id":run_id,"task_revision":task.requirements_revision,
            "stop":execution.stop,"snapshot":again,"approved_local_tools":approved,"permission_submissions":submissions,
            "controller_denials":controller_denials,"failed_local_tools_without_requests":failed_local_tools_without_requests,"workspace":workspace,"history":replay});
        let transcript = crate::goal_artifacts::put(&root, &serde_json::to_vec(&evidence)?).await?;
        Ok((workspace, transcript))
    }
}

fn unopened_startup_history(replay: &ReplayBatch) -> Result<()> {
    anyhow::ensure!(
        replay.history_complete && replay.first_retained_seq == 1 && !replay.events.is_empty(),
        "startup transcript is incomplete"
    );
    let mut sequence = 0_u64;
    for event in &replay.events {
        anyhow::ensure!(
            event.seq == sequence.saturating_add(1)
                && matches!(
                    event.kind,
                    SessionEventKind::Lifecycle { .. }
                        | SessionEventKind::TurnFailed { .. }
                        | SessionEventKind::Lease { .. }
                        | SessionEventKind::Review { .. }
                        | SessionEventKind::Activity { .. }
                ),
            "startup history includes a session, prompt, update, permission or gap"
        );
        sequence = event.seq;
    }
    anyhow::ensure!(sequence == replay.snapshot_seq, "startup cutoff changed");
    Ok(())
}

fn controller_denial<'a>(
    replay: &'a ReplayBatch,
    permission: &crate::supervisor::PendingPermissionSnapshot,
) -> Result<Option<&'a crate::supervisor::SessionEvent>> {
    use crate::supervisor::PermissionResolutionOrigin;
    let requests = replay.events.iter().filter(|event| matches!(&event.kind,
        SessionEventKind::Permission { permission: original } if original.permission_id == permission.permission_id)).collect::<Vec<_>>();
    anyhow::ensure!(
        requests.len() == 1,
        "permission identity is missing or reused"
    );
    let resolutions = replay.events.iter().filter(|event| matches!(&event.kind,
        SessionEventKind::PermissionResolved { permission_id, .. } if permission_id == &permission.permission_id)).collect::<Vec<_>>();
    anyhow::ensure!(
        resolutions.len() <= 1,
        "permission has conflicting resolution evidence"
    );
    let Some(event) = resolutions.first() else {
        return Ok(None);
    };
    anyhow::ensure!(
        event.seq > requests[0].seq,
        "permission resolution predates the request"
    );
    let SessionEventKind::PermissionResolved {
        option_id, origin, ..
    } = &event.kind
    else {
        return Ok(None);
    };
    if !matches!(
        origin,
        Some(PermissionResolutionOrigin::Teardown | PermissionResolutionOrigin::Cancellation)
    ) {
        return Ok(None);
    }
    let denied = match option_id {
        None => true,
        Some(id) => permission.options.iter().any(|option| {
            option.option_id.0.as_ref() == id
                && matches!(
                    option.kind,
                    agent_client_protocol::schema::v1::PermissionOptionKind::RejectOnce
                )
        }),
    };
    Ok(denied.then_some(*event))
}

fn completed_local_tools(
    replay: &ReplayBatch,
    approved: &BTreeSet<String>,
    failed_without_requests: &BTreeSet<String>,
) -> Result<()> {
    let mut denied = BTreeMap::new();
    for event in &replay.events {
        if let SessionEventKind::Permission { permission } = &event.kind
            && let Some(resolution) = controller_denial(replay, permission)?
        {
            let id = permission.tool_call.tool_call_id.0.to_string();
            anyhow::ensure!(
                !approved.contains(&id) && denied.insert(id, resolution.seq).is_none(),
                "denied tool identity is reused or had prior consent"
            );
        }
    }
    anyhow::ensure!(failed_without_requests.iter().all(|id| !approved.contains(id)
        && !denied.contains_key(id) && !replay.events.iter().any(|event| matches!(&event.kind,
            SessionEventKind::Permission { permission } if permission.tool_call.tool_call_id.0.as_ref() == id))),
        "an offered permission cannot be bypassed as an unrequested failure");
    let mut tools = BTreeMap::<String, (String, String)>::new();
    for event in &replay.events {
        anyhow::ensure!(
            !matches!(event.kind, SessionEventKind::PayloadOmitted { .. }),
            "tool material was omitted from history"
        );
        let SessionEventKind::Update { update } = &event.kind else {
            continue;
        };
        let update = serde_json::to_value(update)?;
        if matches!(
            update["sessionUpdate"].as_str(),
            Some("tool_call" | "tool_call_update")
        ) {
            if update["sessionUpdate"] == "tool_call" {
                anyhow::ensure!(
                    update["kind"].as_str().is_some(),
                    "initial tool kind is unspecified; local effects cannot be established"
                );
            }
            let id = update["toolCallId"]
                .as_str()
                .context("tool history omitted its identity")?;
            anyhow::ensure!(
                approved.contains(id)
                    || denied.contains_key(id)
                    || failed_without_requests.contains(id),
                "tool execution has no matching scoped permission"
            );
            if update["sessionUpdate"] == "tool_call" {
                anyhow::ensure!(!tools.contains_key(id), "tool identity was reused");
            }
            if let Some(resolution_seq) = denied.get(id) {
                anyhow::ensure!(
                    update["status"].as_str() != Some("completed"),
                    "denied tool reported completed effects"
                );
                anyhow::ensure!(
                    update["status"].as_str() != Some("failed") || event.seq > *resolution_seq,
                    "tool failure predates controller refusal"
                );
            }
            let tool = tools.entry(id.into()).or_default();
            if let Some(kind) = update["kind"].as_str() {
                anyhow::ensure!(
                    matches!(kind, "read" | "edit" | "search"),
                    "tool history includes nonlocal or unsupported effects"
                );
                tool.0 = kind.into();
            }
            if let Some(status) = update["status"].as_str() {
                anyhow::ensure!(
                    !matches!(tool.1.as_str(), "completed" | "failed") || tool.1 == status,
                    "tool terminal status regressed or changed"
                );
                tool.1 = status.into();
            }
        }
    }
    anyhow::ensure!(
        !tools.is_empty()
            && tools.len() == approved.len() + denied.len() + failed_without_requests.len(),
        "finished local tool evidence is missing"
    );
    for (id, (kind, status)) in &tools {
        anyhow::ensure!(
            matches!(kind.as_str(), "read" | "edit" | "search")
                && matches!(status.as_str(), "completed" | "failed"),
            "tool effects are external, unsupported or still unresolved"
        );
        anyhow::ensure!(
            !(denied.contains_key(id) || failed_without_requests.contains(id))
                || status == "failed",
            "denied tool has no failed terminal evidence"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unrequested_failed_local_work_is_not_consent_or_proof_of_no_effects() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let path = workspace.path().join("partial.txt");
        tokio::fs::write(&path, "possible partial local effect").await?;
        let value = serde_json::json!({"first_retained_seq":1,"snapshot_seq":2,"history_complete":true,"events":[
            {"seq":1,"at":"2026-10-15T00:00:00Z","kind":{"event":"update","update":{
                "sessionUpdate":"tool_call","toolCallId":"failed-local","title":"Edit","kind":"edit","status":"in_progress","rawInput":{"file_path":path}}}},
            {"seq":2,"at":"2026-10-15T00:00:01Z","kind":{"event":"update","update":{
                "sessionUpdate":"tool_call_update","toolCallId":"failed-local","status":"failed"}}}
        ]});
        let replay: ReplayBatch = serde_json::from_value(value.clone())?;
        let failures = crate::goal_permission_material::failed_without_requests(&replay)?;
        assert_eq!(failures.len(), 1);
        assert!(permissions::is_local_tool(&failures[0], workspace.path()).await?);
        let ids = BTreeSet::from(["failed-local".into()]);
        assert!(completed_local_tools(&replay, &BTreeSet::new(), &ids).is_ok());
        assert!(completed_local_tools(&replay, &BTreeSet::new(), &BTreeSet::new()).is_err());
        assert_eq!(
            tokio::fs::read_to_string(&path).await?,
            "possible partial local effect"
        );
        for mutation in [
            "completed",
            "pending",
            "gap",
            "omitted",
            "kind",
            "changed-path",
            "reused",
        ] {
            let mut changed = value.clone();
            match mutation {
                "completed" | "pending" => {
                    changed["events"][1]["kind"]["update"]["status"] = mutation.into()
                }
                "gap" => changed["events"][1]["seq"] = 3.into(),
                "omitted" => changed["history_complete"] = false.into(),
                "kind" => changed["events"][0]["kind"]["update"]["kind"] = "execute".into(),
                "changed-path" => {
                    changed["events"][1]["kind"]["update"]["rawInput"] =
                        serde_json::json!({"file_path":"another"})
                }
                "reused" => {
                    let mut event = changed["events"][0].clone();
                    event["seq"] = 3.into();
                    changed["events"]
                        .as_array_mut()
                        .context("events missing")?
                        .push(event);
                    changed["snapshot_seq"] = 3.into();
                }
                _ => anyhow::bail!("unknown mutation"),
            }
            assert!(
                crate::goal_permission_material::failed_without_requests(&serde_json::from_value(
                    changed
                )?)
                .is_err(),
                "{mutation}"
            );
        }
        let mut requested = value;
        let permission = serde_json::json!({"seq":2,"at":"2026-10-15T00:00:00Z","kind":{"event":"permission","permission":{
            "permission_id":"actual-request","tool_call":{"toolCallId":"failed-local","title":"Edit","kind":"edit","rawInput":{"file_path":path}},
            "options":[{"optionId":"once","name":"Allow once","kind":"allow_once"}]}}});
        requested["events"]
            .as_array_mut()
            .context("events missing")?
            .insert(1, permission);
        requested["events"][2]["seq"] = 3.into();
        requested["snapshot_seq"] = 3.into();
        let replay: ReplayBatch = serde_json::from_value(requested)?;
        assert!(crate::goal_permission_material::failed_without_requests(&replay)?.is_empty());
        assert!(completed_local_tools(&replay, &BTreeSet::new(), &ids).is_err());
        Ok(())
    }

    #[test]
    fn unopened_startup_rejects_prompt_updates_gaps_and_missing_history() -> Result<()> {
        let value = serde_json::json!({"first_retained_seq":1,"snapshot_seq":2,"history_complete":true,"events":[
            {"seq":1,"at":"2026-10-15T00:00:00Z","kind":{"event":"lifecycle","process":"starting"}},
            {"seq":2,"at":"2026-10-15T00:00:01Z","kind":{"event":"turn_failed","message":"initialize failed"}}
        ]});
        assert!(unopened_startup_history(&serde_json::from_value(value.clone())?).is_ok());
        for mutation in [
            "missing", "gap", "cutoff", "omitted", "prompt", "update", "session",
        ] {
            let mut changed = value.clone();
            match mutation {
                "missing" => changed["history_complete"] = false.into(),
                "gap" => changed["events"][1]["seq"] = 3.into(),
                "cutoff" => changed["snapshot_seq"] = 3.into(),
                "prompt" => {
                    changed["events"][1]["kind"] =
                        serde_json::json!({"event":"user_prompt","text":"already admitted"})
                }
                "update" => {
                    changed["events"][1]["kind"] = serde_json::json!({"event":"update","update":{
                    "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"actual work"}}})
                }
                "session" => {
                    changed["events"][1]["kind"] = serde_json::json!({"event":"native_session_selected",
                    "native_session_id":"opened","agent_session_id":null,"initial_settings":{"modes":null,"config_options":null}})
                }
                _ => {
                    changed["events"][1]["kind"] = serde_json::json!({"event":"payload_omitted","description":"tool material omitted"})
                }
            }
            let replay: ReplayBatch = serde_json::from_value(changed)?;
            assert!(
                unopened_startup_history(&replay).is_err(),
                "unsafe startup history accepted: {mutation}"
            );
        }
        Ok(())
    }

    #[test]
    fn denied_local_tool_requires_actual_rejection_origin_and_failed_terminal() -> Result<()> {
        let value = serde_json::json!({"first_retained_seq":1,"snapshot_seq":4,"history_complete":true,"events":[
            {"seq":1,"at":"2026-10-14T00:00:00Z","kind":{"event":"update","update":{
                "sessionUpdate":"tool_call","toolCallId":"local-read","title":"Read local input","kind":"read","status":"in_progress","locations":[{"path":"input.txt"}]}}},
            {"seq":2,"at":"2026-10-14T00:00:01Z","kind":{"event":"permission","permission":{
                "permission_id":"actual-request","tool_call":{"toolCallId":"local-read","title":"Read local input","kind":"read","locations":[{"path":"input.txt"}]},
                "options":[{"optionId":"deny-once","name":"Reject once","kind":"reject_once"}]}}},
            {"seq":3,"at":"2026-10-14T00:00:02Z","kind":{"event":"permission_resolved","permission_id":"actual-request","option_id":"deny-once","automatic":false,"origin":"teardown"}},
            {"seq":4,"at":"2026-10-14T00:00:03Z","kind":{"event":"update","update":{
                "sessionUpdate":"tool_call_update","toolCallId":"local-read","status":"failed"}}}
        ]});
        let replay: ReplayBatch = serde_json::from_value(value.clone())?;
        assert!(completed_local_tools(&replay, &BTreeSet::new(), &BTreeSet::new()).is_ok());
        for mutation in [
            "legacy",
            "unoffered",
            "unfinished",
            "completed",
            "execute",
            "reused",
            "failure-before-rejection",
        ] {
            let mut changed = value.clone();
            match mutation {
                "legacy" => {
                    changed["events"][2]["kind"]
                        .as_object_mut()
                        .context("resolution not an object")?
                        .remove("origin");
                }
                "unoffered" => changed["events"][2]["kind"]["option_id"] = "invented".into(),
                "unfinished" => {
                    changed["events"][3]["kind"]["update"]["status"] = "in_progress".into()
                }
                "completed" => {
                    changed["events"][3]["kind"]["update"]["status"] = "completed".into()
                }
                "execute" => changed["events"][0]["kind"]["update"]["kind"] = "execute".into(),
                "reused" => {
                    let mut start = changed["events"][0].clone();
                    start["seq"] = 5.into();
                    changed["events"]
                        .as_array_mut()
                        .context("events missing")?
                        .push(start);
                    changed["snapshot_seq"] = 5.into();
                }
                "failure-before-rejection" => {
                    changed["events"]
                        .as_array_mut()
                        .context("events missing")?
                        .swap(2, 3);
                    changed["events"][2]["seq"] = 3.into();
                    changed["events"][3]["seq"] = 4.into();
                }
                _ => anyhow::bail!("unexpected fixture mutation"),
            }
            assert!(
                completed_local_tools(
                    &serde_json::from_value(changed)?,
                    &BTreeSet::new(),
                    &BTreeSet::new()
                )
                .is_err(),
                "{mutation}"
            );
        }
        Ok(())
    }

    #[test]
    fn later_local_kind_cannot_hide_prior_execute_or_fetch() -> Result<()> {
        for kind in ["execute", "fetch", "other", "edit"] {
            let replay: ReplayBatch = serde_json::from_value(serde_json::json!({
                "first_retained_seq":1,"snapshot_seq":2,"history_complete":true,"events":[
                    {"seq":1,"at":"2026-10-13T00:00:00Z","kind":{"event":"update","update":{
                        "sessionUpdate":"tool_call","toolCallId":"actual-tool","title":"Original operation","kind":kind,"status":"in_progress"}}},
                    {"seq":2,"at":"2026-10-13T00:00:01Z","kind":{"event":"update","update":{
                        "sessionUpdate":"tool_call_update","toolCallId":"actual-tool","kind":"edit","status":"completed"}}}
                ]
            }))?;
            assert_eq!(
                completed_local_tools(
                    &replay,
                    &BTreeSet::from(["actual-tool".into()]),
                    &BTreeSet::new()
                )
                .is_ok(),
                kind == "edit",
                "original kind: {kind}"
            );
        }
        Ok(())
    }
}
