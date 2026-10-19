//! Bounded read-only projections of existing native execution and model facts.
use super::*;
use bitrouter_orchestrator::goal::accounting::{Event, Work, WorkKind};
use bitrouter_orchestrator::thread::{GoalWork, ThreadHistoryRequest, ThreadTarget};
use bitrouter_sdk::language_model::native::{NativeAttemptReport, NativePlan};
use bitrouter_sdk::language_model::native_preparation::{
    NativePreparationWork, NativePreparationWorkReport,
};
use sha2::{Digest, Sha256};

const HISTORY_WINDOW: u64 = 1024;
const MAX_MODEL_WORK: usize = 32;
const MAX_REPORT_BYTES: u64 = 2 * 1024 * 1024;

impl Coordinator {
    pub(super) async fn native_execution_evidence(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<serde_json::Value> {
        anyhow::ensure!(
            goal.workflow
                .tasks
                .iter()
                .any(|known| known.spec == task.spec && known.executions.contains(execution)),
            "native execution is outside this Goal cutoff"
        );
        self.native_execution_evidence_bound(goal, task, execution)
            .await
    }

    // Callers establish live membership or verify an immutable archive receipt.
    pub(super) async fn native_execution_evidence_bound(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<serde_json::Value> {
        self.native_workspace(goal, execution)?;
        anyhow::ensure!(
            task.execution_harness(execution).is_none(),
            "execution requested ACP"
        );
        let binding = execution
            .thread
            .as_ref()
            .context("native Thread/Turn link missing")?;
        let target = ThreadTarget {
            thread_id: binding.thread_id.clone(),
            server_instance_id: self.threads.capabilities().server_instance_id,
        };
        let caller = CallerContext::local();
        let view = self
            .threads
            .read_stored_thread_view(&target, &caller)
            .await?;
        let source = super::native::source(goal, task, execution)?;
        anyhow::ensure!(
            view.thread.goal_work.as_ref() == Some(&source)
                && view.thread.workspace.canonicalize()? == execution.workspace.canonicalize()?,
            "native execution source or workspace differs"
        );
        let turn = self
            .threads
            .read_stored_turn(&target, &caller, &binding.turn_id)
            .await?;
        let cutoff = view.thread.cursor;
        let start = cutoff.saturating_sub(HISTORY_WINDOW);
        let mut after = start;
        let mut observations = VecDeque::new();
        let mut omitted = 0usize;
        loop {
            let from = after;
            let page = self
                .threads
                .thread_history(
                    &target,
                    &caller,
                    ThreadHistoryRequest {
                        after,
                        cutoff: Some(cutoff),
                        limit: 128,
                    },
                )
                .await?;
            anyhow::ensure!(
                page.thread_id == binding.thread_id && page.cutoff == cutoff,
                "native observation cutoff differs"
            );
            for event in page.events {
                anyhow::ensure!(
                    event.seq > after && event.seq <= cutoff,
                    "native history did not advance"
                );
                after = event.seq;
                for change in event.changes {
                    use bitrouter_orchestrator::thread::ThreadChange;
                    let value = match change {
                        ThreadChange::ModelStep {
                            turn_id,
                            step_id,
                            context_version,
                            ..
                        } if turn_id == binding.turn_id => Some(
                            serde_json::json!({"event":"model_input_committed","step_id":step_id,"context_version":context_version}),
                        ),
                        ThreadChange::AssistantResponse {
                            turn_id,
                            step_id,
                            request_id,
                            usage,
                            ..
                        } if turn_id == binding.turn_id => Some(
                            serde_json::json!({"event":"model_output_committed","step_id":step_id,"request_id":request_id,"usage":usage}),
                        ),
                        ThreadChange::ToolIntent {
                            turn_id,
                            step_id,
                            call,
                        } if turn_id == binding.turn_id => Some(
                            serde_json::json!({"event":"tool_intent_committed","step_id":step_id,"item_id":call.item_id,"tool":call.name}),
                        ),
                        ThreadChange::ToolResult {
                            turn_id,
                            step_id,
                            item_id,
                            effect,
                            message,
                            ..
                        } if turn_id == binding.turn_id => Some(
                            serde_json::json!({"event":"tool_result_committed","step_id":step_id,"item_id":item_id,"effect":effect,
                                "reported_errors":message.content.iter().filter_map(|content| match content {
                                    bitrouter_ai::types::Content::ToolResult { output,.. } => Some(output.is_error()),
                                    _ => None,
                                }).collect::<Vec<_>>()}),
                        ),
                        ThreadChange::ContextRouting {
                            turn_id,
                            inspection,
                        } if turn_id == binding.turn_id => Some(
                            serde_json::json!({"event":"context_routing_committed","inspection":inspection}),
                        ),
                        _ => None,
                    };
                    if let Some(mut value) = value {
                        value["seq"] = event.seq.into();
                        value["committed_at_ms"] = event.timestamp_ms.into();
                        observations.push_back(value);
                        if observations.len() > 32 {
                            observations.pop_front();
                            omitted += 1;
                        }
                    }
                }
            }
            let Some(next) = page.next_after else { break };
            anyhow::ensure!(
                next >= after && next > from,
                "native history cursor stalled"
            );
            after = next;
        }
        anyhow::ensure!(
            self.threads
                .read_stored_thread_view(&target, &caller)
                .await?
                .thread
                .cursor
                == cutoff,
            "native observations changed while collecting evidence"
        );
        let model_work = Box::pin(model_work(
            &self.goals,
            &crate::goal_artifacts::root(&self.workspaces, &goal.id),
            goal,
            &source,
        ))
        .await?;
        let mut result = serde_json::json!({"status":"observed","executor":"built_in","goal_id":goal.id,
            "task_id":task.spec.id,"execution_id":execution.id,"thread_id":binding.thread_id,"turn_id":binding.turn_id,
            "execution_requirements_revision":execution.requirements_revision,"execution_task_revision":execution.task_revision,
            "current_task_version":execution.task_revision == task.requirements_revision,
            "turn_status":turn.status,"unknown_effect":turn.unknown_effect,"verification":turn.verification,
            "requested_model_route":execution.resources.as_ref().and_then(|resources| resources.selection.model_route.as_ref()),
            "recorded_model_selector":turn.model,"accepted_thread_configuration":view.config,
            "supplied_resources":execution.resources,"model_work":model_work,
            "snapshot_seq":cutoff,"history_after":start,"history_complete":false,
            "public_history_scan_complete":start == 0 && omitted == 0,
            "history_scope":"Selected public Thread observations; not the full native Core/context history",
            "observations":observations,"omitted_observations":omitted,"timeline_complete":false,
            "admission_is_execution_evidence":false,
            "scope":"Existing committed Thread history and hash-verified Goal accounting facts for this exact work source. SDK serving identities are reports, not physical provider attestation. Input/intent commits do not prove tool execution; reports do not prove adoption, verification or permissions. Commit times are not actual execution intervals and cannot prove parallel overlap. Core still prepares context per step. Supplied resources do not prove every skill/MCP tool was used. Missing, omitted or private facts remain unknown."});
        if serde_json::to_vec(&result)?.len() > 96 * 1024 {
            result["omitted_observations"] = (omitted + observations.len()).into();
            result["observations"] = serde_json::json!([]);
            result["history_complete"] = false.into();
            result["public_history_scan_complete"] = false.into();
            result["observation_detail"] = "Observation projection exceeded its byte bound; original Thread history is retained".into();
        }
        anyhow::ensure!(
            serde_json::to_vec(&result)?.len() <= 96 * 1024,
            "native evidence exceeds its projection byte bound; original history/receipts are retained"
        );
        Ok(result)
    }
}

pub(super) async fn model_work(
    goals: &GoalService,
    root: &Path,
    goal: &GoalSnapshot,
    source: &GoalWork,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        source.goal_id == goal.id
            && goal.workflow.owner_thread_id.as_ref() == Some(&source.owner_thread_id),
        "model work source belongs to another Goal owner"
    );
    let start = goal.revision.saturating_sub(HISTORY_WINDOW);
    let mut after = start;
    let mut intents = BTreeMap::<String, Work>::new();
    let mut results = VecDeque::new();
    let mut omitted = 0usize;
    loop {
        let from = after;
        let GoalReply::History { page } = goals
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
            .await?
        else {
            anyhow::bail!("model work history unavailable")
        };
        anyhow::ensure!(page.cutoff == goal.revision, "model history cutoff differs");
        for operation in page.operations {
            anyhow::ensure!(
                operation.goal_id == goal.id
                    && operation.revision > after
                    && operation.revision <= goal.revision,
                "model work history source or cursor differs"
            );
            after = operation.revision;
            let bitrouter_orchestrator::goal::OperationKind::Workflow { change } = operation.kind
            else {
                continue;
            };
            let Change::Accounting { event } = *change else {
                continue;
            };
            match event {
                Event::Begin { work } => {
                    intents.insert(work.id.clone(), work);
                }
                Event::Observe { id, report, .. } => {
                    let work = match intents.remove(&id) {
                        Some(work) => work,
                        None => {
                            // The intent may precede this bounded history tail.
                            // Inspect its original immutable receipt; never create one.
                            let GoalReply::Accepted { operation } = goals
                                .execute(
                                    "local",
                                    GoalCommand::Operation {
                                        operation_id: format!("{id}-begin"),
                                    },
                                    now()?,
                                )
                                .await?
                            else {
                                anyhow::bail!("model intent unavailable")
                            };
                            anyhow::ensure!(
                                operation.goal_id == goal.id && operation.revision <= goal.revision,
                                "model intent is outside the Goal cutoff"
                            );
                            let bitrouter_orchestrator::goal::OperationKind::Workflow { change } =
                                operation.kind
                            else {
                                anyhow::bail!("model intent receipt kind differs")
                            };
                            let Change::Accounting {
                                event: Event::Begin { work },
                            } = *change
                            else {
                                anyhow::bail!("model intent is not an original Begin")
                            };
                            anyhow::ensure!(work.id == id, "model intent identity differs");
                            work
                        }
                    };
                    if work.source.as_deref() == Some(source) {
                        results.push_back((work, Some(report)));
                        if results.len() > MAX_MODEL_WORK {
                            results.pop_front();
                            omitted += 1;
                        }
                    }
                }
            }
        }
        let Some(next) = page.next_after else { break };
        anyhow::ensure!(next > from && next >= after, "model history cursor stalled");
        after = next;
    }
    for work in &goal.accounting.in_flight {
        if work.source.as_deref() == Some(source) {
            results.push_back((work.clone(), None));
            if results.len() > MAX_MODEL_WORK {
                results.pop_front();
                omitted += 1;
            }
        }
    }
    let mut entries = Vec::new();
    for (work, report) in results {
        let observation = match Box::pin(observe(root, &work, report.as_ref())).await {
            Ok(value) => value,
            Err(error) => {
                serde_json::json!({"status":"unavailable","reason":bounded_error(&error)})
            }
        };
        entries.push(
            serde_json::json!({"work_id":work.id,"source":work.source,"kind":work.kind,
            "requirements_revision":work.requirements_revision,"request_id":work.request_id,
            "intent":work.intent,"report":report,"observation":observation}),
        );
    }
    let mut result = serde_json::json!({"goal_cutoff":goal.revision,"history_after":start,
        "history_complete":start == 0 && omitted == 0,"omitted_work":omitted,"entries":entries,
        "scope":"Bounded original accounting receipts; input intent, route admission and actual SDK outcomes are separate. Complete original records/artifacts remain available by receipt identity. No missing usage is converted to zero."});
    if serde_json::to_vec(&result)?.len() > 64 * 1024 {
        result["omitted_work"] = (omitted + entries.len()).into();
        result["entries"] = serde_json::json!([]);
        result["history_complete"] = false.into();
        result["detail"] =
            "Model projection exceeded its byte bound; original receipts are retained".into();
    }
    Ok(result)
}

async fn read(root: &Path, reference: &ArtifactRef) -> Result<Vec<u8>> {
    anyhow::ensure!(
        reference.bytes <= MAX_REPORT_BYTES,
        "model artifact exceeds projection byte bound"
    );
    crate::goal_artifacts::read(root, reference).await
}

async fn observe(
    root: &Path,
    work: &Work,
    report: Option<&ArtifactRef>,
) -> Result<serde_json::Value> {
    match work.kind {
        WorkKind::Preparation => {
            let value: serde_json::Value =
                serde_json::from_slice(&read(root, &work.intent).await?)?;
            // Existing accounting uses Preparation for both callback work and
            // prepared model plans. A callback is not a provider attempt.
            if value.get("routes").is_none() {
                let input: NativePreparationWork = serde_json::from_value(value)?;
                anyhow::ensure!(
                    work.request_id.as_ref() == Some(&input.request_id),
                    "preparation request identity differs"
                );
                let outcome = match report {
                    Some(reference) => Some(serde_json::from_slice::<NativePreparationWorkReport>(
                        &read(root, reference).await?,
                    )?),
                    None => None,
                };
                anyhow::ensure!(
                    outcome.as_ref().is_none_or(|report| report.work == input),
                    "preparation outcome identity differs"
                );
                return Ok(
                    serde_json::json!({"status":"observed","stage":"preparation_callback",
                    "category":input.kind,"outcome_reported":outcome.is_some(),
                    "elapsed_ms":outcome.as_ref().map(|report| report.elapsed_ms),
                    "error_code":outcome.as_ref().and_then(|report| report.error_code.as_ref()),
                    "actual_provider":null,"actual_model":null}),
                );
            }
            let plan: NativePlan = serde_json::from_value(value)?;
            anyhow::ensure!(
                work.request_id.as_ref() == Some(&plan.request_id),
                "plan request identity differs"
            );
            let prompt = serde_json::to_vec(&plan.prompt)?;
            let admission = match report {
                Some(reference) => Some(serde_json::from_slice::<serde_json::Value>(
                    &read(root, reference).await?,
                )?),
                None => None,
            };
            Ok(
                serde_json::json!({"status":"observed","stage":"prepared_input",
                "requested_selector":plan.original_model,"effective_model":plan.effective_model,"router":plan.router,
                "prepared_prompt_sha256":hex::encode(Sha256::digest(&prompt)),"prepared_prompt_bytes":prompt.len(),
                "fallback_candidates":plan.routes.iter().map(|route| serde_json::json!({"provider":route.provider,"model":route.model,"protocol":route.protocol})).collect::<Vec<_>>(),
                "admitted_route_indices":admission.as_ref().map(|value| &value["accepted_routes"]),
                "actual_provider":null,"actual_model":null}),
            )
        }
        WorkKind::ProviderAttempt => {
            let Some(reference) = report else {
                return Ok(
                    serde_json::json!({"status":"pending","actual_provider":null,"actual_model":null}),
                );
            };
            let value: serde_json::Value = serde_json::from_slice(&read(root, reference).await?)?;
            let activity = value
                .get("bro_stream_activity")
                .and_then(|value| {
                    serde_json::from_value::<super::model_control::activity::StreamActivity>(
                        value.clone(),
                    )
                    .ok()
                })
                .filter(|activity| activity.valid());
            let activity_status = if activity.is_some() {
                "observed"
            } else if value.get("bro_stream_activity").is_some() {
                "unavailable"
            } else {
                "not_recorded"
            };
            let report: NativeAttemptReport = serde_json::from_value(value)?;
            let intent: serde_json::Value =
                serde_json::from_slice(&read(root, &work.intent).await?)?;
            anyhow::ensure!(
                work.request_id.as_ref() == Some(&report.request_id)
                    && intent["request_id"] == report.request_id
                    && intent["attempt_index"] == report.attempt_index
                    && intent["route"] == serde_json::to_value(&report.route)?,
                "actual report differs from original request/route"
            );
            Ok(
                serde_json::json!({"status":"observed","stage":"sdk_outcome","attempt_index":report.attempt_index,
                "candidate_provider":report.route.provider,"candidate_model":report.route.model,"protocol":report.route.protocol,
                "actual_provider":report.actual_provider,"actual_model":report.actual_model,
                "usage":report.result.as_ref().and_then(|result| result.usage.as_ref()),
                "output_recorded":report.result.is_some(),"output_rejected":report.output_rejection.is_some(),
                "report_rejected":report.report_rejection.is_some(),"error_reported":report.error.is_some(),"elapsed_ms":report.elapsed_ms,
                "stream_activity":activity,
                "activity_status":activity_status,
                "activity_scope":"Local receipt of content fragments only. Compare monotonic ticks only within the same clock_id; wall timestamps may regress. Not admission, full execution lifetime, physical provider timing, tool execution or settlement"}),
            )
        }
        _ => Ok(
            serde_json::json!({"status":if report.is_some() { "outcome_reference_retained" } else { "pending" },
            "scope":"Auxiliary work; this reference is not a generated model response or adoption proof"}),
        ),
    }
}
