//! Native model/tool execution, Goal verification and SQLite; provider verdicts
//! are fixtures, not evidence of real-model planning or review quality.
use super::*;
use serde_json::{Value, json};
use wiremock::{Request, ResponseTemplate};

pub(super) fn completion(
    body: &Value,
    mut message: Value,
    finish_reason: &str,
) -> ResponseTemplate {
    if body["stream"] == true {
        for (index, call) in message["tool_calls"]
            .as_array_mut()
            .into_iter()
            .flatten()
            .enumerate()
        {
            call["index"] = index.into();
        }
        let delta = json!({"id":"native-goal-response", "object":"chat.completion.chunk", "model":"test-model",
            "choices":[{"index":0,"delta":message,"finish_reason":null}]});
        let finished = json!({"id":"native-goal-response", "object":"chat.completion.chunk", "model":"test-model",
            "choices":[{"index":0,"delta":{},"finish_reason":finish_reason}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}});
        return ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(format!(
                "data: {delta}\n\ndata: {finished}\n\ndata: [DONE]\n\n"
            ));
    }
    ResponseTemplate::new(200).set_body_json(json!({"id":"native-goal-response", "object":"chat.completion",
        "model":"test-model", "choices":[{"index":0,"message":message,"finish_reason":finish_reason}],
        "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}))
}

fn model(request: &Request) -> ResponseTemplate {
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return ResponseTemplate::new(400);
    };
    let text = String::from_utf8_lossy(&request.body);
    let writing = text.contains("NATIVE_WRITE_FIXTURE");
    let shell = text.contains("NATIVE_SHELL_FIXTURE");
    let file = if shell {
        "shell-result.txt"
    } else if writing {
        "native-result.txt"
    } else {
        "seed.txt"
    };
    if body["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty())
    {
        let read_observed = body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["role"] == "tool"
                    && (writing
                        || shell
                        || message["content"].to_string().contains("original input"))
            })
        });
        return if read_observed {
            completion(
                &body,
                json!({"role":"assistant", "content":if writing || shell {
                    "Native operation returned; BRO must independently verify the actual output file."
                } else { "Observed seed.txt contains original input. No file changes were required." }}),
                "stop",
            )
        } else {
            let (name, arguments) = if shell {
                (
                    "shell",
                    json!({"command":"printf 'NATIVE_GOAL\\n' >> shell-result.txt"}),
                )
            } else if writing {
                ("write", json!({"path":file,"content":"NATIVE_GOAL\n"}))
            } else {
                ("read", json!({"path":"seed.txt"}))
            };
            completion(
                &body,
                json!({"role":"assistant", "tool_calls":[{"id":"native-operation", "type":"function",
                "function":{"name":name, "arguments":arguments.to_string()}}]}),
                "tool_calls",
            )
        };
    }
    let Some(state) = provider::model_state(request) else {
        return ResponseTemplate::new(400);
    };
    let check = |id| {
        json!({"id":id,"description":"Verify the original seed content",
        "recipe":{"kind":"file_contains","path":file,"text":if writing || shell { "NATIVE_GOAL" } else { "original input" }},"timeout_seconds":10})
    };
    let condition = if writing || shell {
        "operation"
    } else {
        "inspection"
    };
    let final_check = if writing || shell {
        "operation-final"
    } else {
        "seed-final"
    };
    let instruction = if shell {
        "NATIVE_SHELL_FIXTURE: run the requested shell command and wait for its actual permission."
    } else if writing {
        "NATIVE_WRITE_FIXTURE: write native-result.txt containing NATIVE_GOAL newline with the write tool."
    } else {
        "Read seed.txt with the read tool and report its contents."
    };
    let result = if state.get("available_harnesses").is_some() {
        if state["built_in_executor_available"] != true || state["available_harnesses"] != json!([])
        {
            return ResponseTemplate::new(400);
        }
        json!({"tasks":[{"id":"inspect", "title":"Inspect seed", "instruction":instruction,
            "harness":null,"dependencies":[],"checks":[check("seed-task")]}],
            "completion":{"conditions":[{"id":condition,"description":"Deliver the requested verified native operation result"}],
                "checks":[check(final_check)],"human_acceptance_required":false}})
    } else if state.get("actual_workspace").is_some() {
        json!({"ready":true,"reason":"Fixture semantic judgment; machine checks are independently required", "missing_checks":[]})
    } else if state.get("integrated_material").is_some() {
        let runtime = &state["task_evidence"][0]["runtime_observations"];
        let provider_observed =
            runtime["model_work"]["entries"]
                .as_array()
                .is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry["kind"] == "provider_attempt"
                            && (entry["observation"]["actual_provider"] == "fixture"
                                || entry["observation"]["actual_provider"] == "fixture-backup")
                            && entry["observation"]["actual_model"] == "test-model"
                    })
                });
        if runtime["executor"] != "built_in"
            || !provider_observed
            || runtime["history_complete"] != false
            || runtime["timeline_complete"] != false
        {
            return ResponseTemplate::new(422).set_body_json(json!({
                "status":runtime["status"],"reason":runtime["reason"],
                "model_work":runtime["model_work"]
            }));
        }
        json!({"conditions":[{"id":condition,"status":"met","reason":"Actual read and content-check evidence",
            "evidence":["integrated_material",format!("check:{final_check}")]}],"unmet_original_requirements":[],
            "constraints_preserved":true,"human_acceptance_required":false,"summary":"Verified seed inspection fixture"})
    } else {
        return ResponseTemplate::new(400);
    };
    completion(
        &body,
        json!({"role":"assistant","content":result.to_string()}),
        "stop",
    )
}

#[tokio::test]
async fn native_goal_uses_real_thread_turn_and_tools_without_an_external_harness() -> Result<()> {
    let _diagnostics = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_test_writer()
            .finish(),
    );
    let mut setup = fixture::Setup::new_without_external_harnesses().await?;
    setup.persistent_database()?;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(model)
        .with_priority(1)
        .mount(&setup.backend)
        .await;
    let fixture = setup.start().await.context("start native fixture")?;
    let result = async {
        assert!(fixture.routing.snapshot_config().agents.is_empty());
        let id = fixture.create_with("Read seed.txt and deliver a verified report of its observed contents. Do not change the file.").await.context("create native Goal via IPC")?;
        let goal = wait_for(&fixture.socket, &id, |goal| goal.workflow.completion.completed_for(1)).await?;
        let task = goal.workflow.tasks.first().context("native task missing")?;
        assert!(task.spec.harness.is_none());
        let execution = task.executions.last().context("native execution missing")?;
        let binding = execution.thread.as_ref().context("native Thread/Turn binding missing")?;
        let GoalReply::Accepted { operation } = rpc(&fixture.socket, GoalCommand::Operation {
            operation_id: format!("goal-work-{}-binding", execution.id),
        }).await? else { anyhow::bail!("native binding receipt is missing"); };
        let bitrouter_orchestrator::goal::OperationKind::Workflow { change } = operation.kind else { anyhow::bail!("binding receipt has another operation kind"); };
        let Change::BindWorkerTurn { thread, .. } = *change else { anyhow::bail!("binding receipt does not reference a worker Turn"); };
        assert_eq!(&thread, binding);
        assert!(matches!(execution.observation, Observation::Candidate { run_id: None, native_session_id: None, .. }));
        assert!(matches!(execution.stop, Some(StopState::Confirmed { .. })));
        assert!(execution.handoff.is_some());
        let target = bitrouter_orchestrator::thread::ThreadTarget { thread_id: binding.thread_id.clone(),
            server_instance_id: fixture.threads.capabilities().server_instance_id };
        let caller = CallerContext::local();
        let view = fixture.threads.read_stored_thread_view(&target, &caller).await?;
        assert_eq!(view.thread.goal_work.as_ref().map(|source| &source.execution_id), Some(&execution.id));
        let turn = fixture.threads.read_stored_turn(&target, &caller, &binding.turn_id).await?;
        assert_eq!(turn.status, TurnStatus::Completed);
        assert!(!turn.unknown_effect);
        assert!(turn.final_answer.as_ref().is_some_and(|text| text.contains("original input")));
        let original = tokio::fs::read_to_string(fixture.workspace.join("seed.txt")).await?;
        assert_eq!(original, "original input\n");
        let requests = fixture.backend.received_requests().await.context("request capture unavailable")?;
        assert!(requests.iter().any(|request| {
            serde_json::from_slice::<Value>(&request.body).ok().is_some_and(|body| {
                body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|
                    message["role"] == "tool" && message["content"].as_str().is_some_and(|text| text.contains("original input"))))
            })
        }));
        let source = super::super::native::source(&goal, task, execution)?;
        let evidence = super::super::native_evidence::model_work(&fixture.goals, &fixture.root(&id), &goal, &source).await?;
        let entries = evidence["entries"].as_array().context("native model entries missing")?;
        assert!(!entries.is_empty());
        assert!(entries.iter().any(|entry| entry["observation"]["stream_activity"]["content_parts"].as_u64().is_some_and(|count| count > 0)),
            "native completion lacks actual received content evidence");
        let serialized_source = serde_json::to_value(&source)?;
        assert!(entries.iter().all(|entry| entry["source"] == serialized_source));
        assert!(entries.iter().any(|entry| entry["kind"] == "preparation"
            && entry["observation"]["requested_selector"] == "goal-model"
            && entry["observation"]["prepared_prompt_bytes"].as_u64().is_some_and(|bytes| bytes > 0)
            && entry["observation"]["prepared_prompt_sha256"].as_str().is_some_and(|hash| hash.len() == 64)
            && entry["observation"]["actual_model"].is_null()));
        let mut foreign = source.clone();
        foreign.execution_id = "unrelated-attempt".into();
        let foreign_evidence = super::super::native_evidence::model_work(&fixture.goals, &fixture.root(&id), &goal, &foreign).await?;
        assert_eq!(foreign_evidence["entries"], json!([]));
        // An artifact whose immutable identity no longer matches cannot supply
        // an actual serving identity. Restore it after checking this refusal.
        let observed = entries.iter().find(|entry| entry["kind"] == "provider_attempt").context("native provider outcome missing")?;
        let reference: ArtifactRef = serde_json::from_value(observed["report"].clone())?;
        let original = crate::goal_artifacts::read(&fixture.root(&id), &reference).await?;
        let changed = original.iter().enumerate().map(|(index, byte)| if index == 0 { *byte ^ 1 } else { *byte }).collect::<Vec<_>>();
        tokio::fs::write(&reference.path, &changed).await?;
        let refused = super::super::native_evidence::model_work(&fixture.goals, &fixture.root(&id), &goal, &source).await;
        tokio::fs::write(&reference.path, original).await?;
        let refused = refused?;
        let rejected = refused["entries"].as_array().context("refusal entries missing")?.iter()
            .find(|entry| entry["work_id"] == observed["work_id"]).context("original outcome was discarded")?;
        assert_eq!(rejected["observation"]["status"], "unavailable");
        assert!(rejected["observation"]["actual_provider"].is_null());
        assert_eq!(fixture.backend.received_requests().await.context("request capture unavailable")?.len(), requests.len());
        // Prove the opt-in failure snapshot mechanism can retain both stores
        // from this real file-backed Goal/Thread run and reopen them read-only.
        let copy = fixture.controls.join("native-evidence-copy.sqlite");
        fixture::copy_database(&fixture.db, &copy).await?;
        let copied_db = crate::db::connect(&format!("sqlite://{}?mode=ro", copy.display())).await?;
        let copied_goals = GoalService::new(Arc::new(crate::goal_store::DatabaseGoalStore::new(copied_db.clone())));
        let GoalReply::Snapshot { goal: copied } = copied_goals.execute("local", GoalCommand::Get { goal_id:id.clone() }, now()?).await?
            else { anyhow::bail!("retained Goal snapshot missing") };
        assert_eq!(copied.workflow.tasks[0].executions.last().and_then(|execution| execution.thread.as_ref()), Some(binding));
        use bitrouter_orchestrator::store::ExecutionStore as _;
        let copied_store = crate::agent_store::DatabaseExecutionStore::new(copied_db);
        let retained = copied_store.read_records(&binding.thread_id, 0, None, 64, 4 * 1024 * 1024).await.map_err(anyhow::Error::msg)?
            .context("retained execution history missing")?;
        assert!(retained.cutoff > 0);
        assert!(!fixture.controls.join("started-inspect").exists());
        Ok(())
    }.await;
    fixture.finish(result).await
}

#[tokio::test]
async fn native_explicit_denial_is_a_reply_but_never_verifies_the_missing_output() -> Result<()> {
    let fixture = native_fixture().await?;
    let result = async {
        let (id, decision, workspace) = pending_native_shell(&fixture).await?;
        rpc(&fixture.socket, GoalCommand::RespondDecision { operation_id: "native-user-deny".into(), goal_id: id.clone(),
            decision_id: decision.id.clone(), expected_requirements_revision: decision.requirements_revision, option_id: "deny-once".into() }).await?;
        let goal = wait_for(&fixture.socket, &id, |goal| goal.workflow.decisions.iter().any(|known|
            known.id == decision.id && matches!(&known.state,
                bitrouter_orchestrator::goal::decision::PermissionState::Submitted { option_id, .. } if option_id == "deny-once"))
            && goal.workflow.tasks.iter().flat_map(|task| &task.executions).any(|execution|
                execution.checks.iter().any(|check| matches!(check.state,
                    bitrouter_orchestrator::goal::workflow::CheckState::Observed { completed: true, passed: false, .. })))).await?;
        assert!(!goal.workflow.completion.completed_for(1));
        assert!(!workspace.join("shell-result.txt").exists());
        let blocked = wait_for(&fixture.socket, &id, |goal| goal.workflow.tasks.iter()
            .flat_map(|task| &task.executions).any(|execution| matches!(&execution.reconciliation,
                Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Blocked { reason, .. })
                    if reason.contains("shell, MCP or unsupported")))).await?;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let held = head(&fixture.socket, &id).await?;
        assert_eq!(held.workflow.tasks[0].executions.len(), blocked.workflow.tasks[0].executions.len());
        assert_eq!(held.workflow.tasks[0].executions.len(), 1);
        Ok(())
    }.await;
    fixture.finish(result).await
}

async fn native_fixture() -> Result<fixture::Fixture> {
    let setup = fixture::Setup::new_without_external_harnesses().await?;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(model)
        .with_priority(2)
        .mount(&setup.backend)
        .await;
    setup.start().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_fallback_evidence_retains_failed_candidate_and_actual_serving_identity()
-> Result<()> {
    let mut setup = Setup::new_without_external_harnesses().await?;
    setup.native_fallback();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &Request| {
            if serde_json::from_slice::<Value>(&request.body)
                .ok()
                .is_some_and(|body| {
                    body["tools"]
                        .as_array()
                        .is_some_and(|tools| !tools.is_empty())
                })
            {
                ResponseTemplate::new(503).set_body_json(
                    json!({"error":{"message":"controlled primary candidate failure"}}),
                )
            } else {
                model(request)
            }
        })
        .with_priority(1)
        .mount(&setup.backend)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/chat/completions"))
        .respond_with(model)
        .mount(&setup.backend)
        .await;
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create_with("Read seed.txt with the built-in executor, preserve its contents and deliver a verified report. Use the configured fallback when the primary candidate is unavailable.").await?;
        let goal = wait_for(&fixture.socket, &id, |goal| goal.workflow.completion.completed_for(1)).await?;
        let task = &goal.workflow.tasks[0];
        let execution = task.executions.last().context("fallback execution missing")?;
        assert_eq!(task.executions.len(), 1);
        let source = super::super::native::source(&goal, task, execution)?;
        let evidence = super::super::native_evidence::model_work(&fixture.goals, &fixture.root(&id), &goal, &source).await?;
        let entries = evidence["entries"].as_array().context("fallback evidence missing")?;
        let failed = entries.iter().find(|entry| entry["kind"] == "provider_attempt"
            && entry["observation"]["candidate_provider"] == "fixture"
            && entry["observation"]["error_reported"] == true).context("failed candidate was omitted")?;
        assert!(failed["observation"]["actual_provider"].is_null());
        assert!(failed["observation"]["actual_model"].is_null());
        assert!(failed["observation"]["usage"].is_null());
        let served = entries.iter().find(|entry| entry["kind"] == "provider_attempt"
            && entry["request_id"] == failed["request_id"]
            && entry["observation"]["actual_provider"] == "fixture-backup").context("actual fallback outcome missing")?;
        assert_eq!(served["observation"]["actual_model"], "test-model");
        assert_eq!(served["observation"]["output_recorded"], true);
        assert_ne!(served["observation"]["attempt_index"], failed["observation"]["attempt_index"]);
        let requests = fixture.backend.received_requests().await.context("fallback requests missing")?;
        assert!(requests.iter().any(|request| request.url.path() == "/v2/chat/completions"));
        assert_eq!(tokio::fs::read_to_string(fixture.workspace.join("seed.txt")).await?, "original input\n");
        Ok(())
    }.await;
    fixture.finish(result).await
}

#[tokio::test]
async fn native_local_write_uses_a_durable_one_time_policy_choice_and_verified_delivery()
-> Result<()> {
    let fixture = native_fixture().await?;
    let result = async {
        let id = fixture.create_with("NATIVE_WRITE_FIXTURE: create native-result.txt containing NATIVE_GOAL newline using the built-in executor. Verify the actual file and deliver it.").await?;
        let goal = wait_for(&fixture.socket, &id, |goal| goal.workflow.completion.completed_for(1)).await?;
        let task = goal.workflow.tasks.first().context("native task missing")?;
        assert_eq!(task.executions.len(), 1);
        let execution = task.executions.last().context("native execution missing")?;
        assert_eq!(tokio::fs::read_to_string(execution.workspace.join("native-result.txt")).await?, "NATIVE_GOAL\n");
        assert!(!fixture.workspace.join("native-result.txt").exists());
        assert!(execution.handoff.is_some());
        assert!(goal.workflow.decisions.iter().any(|decision| decision.run_id.is_none()
            && matches!(&decision.state, bitrouter_orchestrator::goal::decision::PermissionState::Submitted { option_id, .. } if option_id == "allow-once")
            && decision.options.iter().all(|option| !option.persistent)));
        Ok(())
    }.await;
    fixture.finish(result).await
}

#[tokio::test]
async fn native_failed_check_repairs_from_original_effect_evidence_without_replaying_the_old_turn()
-> Result<()> {
    let fixture = native_fixture().await?;
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            if let Ok(body) = serde_json::from_slice::<Value>(&request.body)
                && body["tools"].as_array().is_some_and(|tools| !tools.is_empty())
                && body["messages"].as_array().is_some_and(|messages| !messages.iter().any(|message| message["role"] == "tool"))
                && first.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                completion(&body, json!({"role":"assistant", "tool_calls":[{"id":"native-operation", "type":"function",
                    "function":{"name":"write", "arguments":json!({"path":"native-result.txt", "content":"WRONG\n"}).to_string()}}]}), "tool_calls")
            } else {
                model(request)
            }
        })
        .with_priority(1)
        .mount(&fixture.backend)
        .await;
    let result = async {
        let id = fixture.create_with("NATIVE_WRITE_FIXTURE: create native-result.txt containing NATIVE_GOAL newline. Verify the actual file; repair a known local failure and deliver the verified result.").await?;
        let goal = wait_for(&fixture.socket, &id, |goal| goal.workflow.completion.completed_for(1)).await?;
        let task = goal.workflow.tasks.first().context("native task missing")?;
        assert_eq!(goal.requirements_revision, 1);
        assert_eq!(task.executions.len(), 2);
        let old = &task.executions[0];
        let new = &task.executions[1];
        assert!(old.effects_reconciled());
        assert_ne!(old.thread, new.thread);
        assert_ne!(old.workspace, new.workspace);
        assert_eq!(tokio::fs::read_to_string(old.workspace.join("native-result.txt")).await?, "WRONG\n");
        assert_eq!(tokio::fs::read_to_string(new.workspace.join("native-result.txt")).await?, "NATIVE_GOAL\n");
        assert!(old.checks.iter().any(|check| matches!(check.state,
            bitrouter_orchestrator::goal::workflow::CheckState::Observed { completed: true, passed: false, .. })));
        let Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Verified { transcript, workspace, .. }) = &old.reconciliation
            else { anyhow::bail!("native original-effect proof missing"); };
        let evidence: Value = serde_json::from_slice(&crate::goal_artifacts::read(&fixture.root(&id), transcript).await?)?;
        assert_eq!(evidence["binding"], serde_json::to_value(&old.thread)?);
        assert_eq!(evidence["workspace"], serde_json::to_value(workspace)?);
        let pages = evidence["history_pages"].as_array().context("original history pages missing")?;
        assert!(!pages.is_empty());
        let mut completed_tools = 0;
        for page in pages {
            let reference: ArtifactRef = serde_json::from_value(page.clone())?;
            let page: bitrouter_orchestrator::thread::ThreadHistoryPage = serde_json::from_slice(
                &crate::goal_artifacts::read(&fixture.root(&id), &reference).await?)?;
            completed_tools += page.events.iter().flat_map(|event| &event.changes).filter(|change|
                matches!(change, bitrouter_orchestrator::thread::ThreadChange::ToolResult {
                    effect: bitrouter_orchestrator::store::EffectStatus::Completed, .. })).count();
        }
        assert_eq!(completed_tools, 1);
        assert!(new.handoff.is_some());
        assert!(!fixture.workspace.join("native-result.txt").exists());
        Ok(())
    }.await;
    fixture.finish(result).await
}

async fn pending_native_shell(
    fixture: &fixture::Fixture,
) -> Result<(
    String,
    bitrouter_orchestrator::goal::decision::PermissionDecision,
    PathBuf,
)> {
    let id = fixture.create_with("NATIVE_SHELL_FIXTURE: run the requested local shell command to append NATIVE_GOAL newline to shell-result.txt, then verify and deliver the actual file. Shell execution requires an explicit permission choice.").await?;
    let goal = wait_for(&fixture.socket, &id, |goal| {
        goal.workflow.decisions.iter().any(|decision| {
            decision.run_id.is_none()
                && decision.state
                    == bitrouter_orchestrator::goal::decision::PermissionState::Pending
        })
    })
    .await?;
    let decision = goal
        .workflow
        .decisions
        .iter()
        .find(|decision| {
            decision.run_id.is_none()
                && decision.state
                    == bitrouter_orchestrator::goal::decision::PermissionState::Pending
        })
        .context("native shell decision missing")?
        .clone();
    let execution = goal
        .workflow
        .tasks
        .iter()
        .flat_map(|task| &task.executions)
        .find(|execution| execution.id == decision.execution_id)
        .context("native shell execution missing")?;
    let bytes = crate::goal_artifacts::read(&fixture.root(&id), &decision.request).await?;
    let request = crate::goal_permission_material::native::Request::decode(&bytes)?;
    assert_eq!(request.input.tool_name, "shell");
    assert_eq!(execution.thread.as_ref(), Some(&request.native));
    assert!(!execution.workspace.join("shell-result.txt").exists());
    Ok((id, decision, execution.workspace.clone()))
}

#[tokio::test]
async fn native_manual_choice_is_persisted_replayed_and_does_not_repeat_the_shell_effect()
-> Result<()> {
    let fixture = native_fixture().await?;
    let result = async {
        let (id, decision, workspace) = pending_native_shell(&fixture).await?;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!workspace.join("shell-result.txt").exists());
        let command = || GoalCommand::RespondDecision {
            operation_id: "native-user-allow".into(),
            goal_id: id.clone(),
            decision_id: decision.id.clone(),
            expected_requirements_revision: decision.requirements_revision,
            option_id: "allow-once".into(),
        };
        let GoalReply::Accepted { operation: first } = rpc(&fixture.socket, command()).await?
        else {
            anyhow::bail!("native choice receipt missing");
        };
        let goal = wait_for(&fixture.socket, &id, |goal| {
            goal.workflow.completion.completed_for(1)
        })
        .await?;
        let GoalReply::Accepted {
            operation: repeated,
        } = rpc(&fixture.socket, command()).await?
        else {
            anyhow::bail!("native choice replay missing");
        };
        assert_eq!(first.id, repeated.id);
        assert_eq!(first.revision, repeated.revision);
        assert_eq!(
            tokio::fs::read_to_string(workspace.join("shell-result.txt")).await?,
            "NATIVE_GOAL\n"
        );
        assert_eq!(goal.workflow.tasks[0].executions.len(), 1);
        assert!(!fixture.workspace.join("shell-result.txt").exists());
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test]
async fn paused_native_permission_cannot_admit_a_late_allow_or_count_cancellation_as_consent()
-> Result<()> {
    let fixture = native_fixture().await?;
    let result = async {
        let (id, decision, workspace) = pending_native_shell(&fixture).await?;
        control(
            &fixture.socket,
            &id,
            "native-pause-before-allow",
            RunIntent::Pause,
        )
        .await?;
        assert!(
            rpc(
                &fixture.socket,
                GoalCommand::RespondDecision {
                    operation_id: "native-late-allow".into(),
                    goal_id: id.clone(),
                    decision_id: decision.id.clone(),
                    expected_requirements_revision: decision.requirements_revision,
                    option_id: "allow-once".into()
                }
            )
            .await
            .is_err()
        );
        let goal = wait_for(&fixture.socket, &id, |goal| {
            goal.workflow.decisions.iter().any(|known| {
                known.id == decision.id
                    && matches!(
                        known.state,
                        bitrouter_orchestrator::goal::decision::PermissionState::Obsolete { .. }
                    )
            })
        })
        .await?;
        assert_eq!(goal.run_intent, RunIntent::Pause);
        assert!(!goal.workflow.completion.completed_for(1));
        assert!(!workspace.join("shell-result.txt").exists());
        assert!(
            !goal
                .workflow
                .decisions
                .iter()
                .any(|known| known.id == decision.id
                    && matches!(
                        known.state,
                        bitrouter_orchestrator::goal::decision::PermissionState::Submitted { .. }
                    ))
        );
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn superseded_native_execution_keeps_receipt_verified_history_without_replay() -> Result<()> {
    let mut setup = fixture::Setup::new_without_external_harnesses().await?;
    setup.persistent_database()?;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(|request: &Request| {
            let Some(state) = provider::model_state(request) else { return model(request) };
            if state["trusted_state"]["input"].as_str().is_some_and(|text| text.contains("NATIVE_HISTORY_REVISION")) {
                let Ok(body) = serde_json::from_slice::<Value>(&request.body) else { return ResponseTemplate::new(400) };
                return completion(&body,json!({"role":"assistant","content":json!({"answer":null,"objective":null,
                    "task_updates":[{"task_id":"inspect","instruction":"Read seed.txt with the read tool and report its unchanged original input. NATIVE_HISTORY_REVISION"}],
                    "control":null,"permission_response":null,"clarification":null}).to_string()}),"stop");
            }
            model(request)
        }).with_priority(1).mount(&setup.backend).await;
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create_with("Read seed.txt and deliver a verified report of its observed contents. Preserve the original input.").await?;
        let first = wait_for(&fixture.socket,&id,|goal| goal.workflow.completion.completed_for(1)).await?;
        let original = first.workflow.tasks[0].executions.last().context("original native execution missing")?.clone();
        let link = original.thread.clone().context("original native binding missing")?;
        fixture.input(&id,"native-history-edit","FIXTURE_NEW_LEFT_RULE NATIVE_HISTORY_REVISION: update only the inspection instructions and retain the original checks.",Some("inspect")).await?;
        let completed = wait_for(&fixture.socket,&id,|goal| goal.workflow.completion.completed_for(2)
            && goal.workflow.tasks[0].archived_executions == 1).await?;
        assert_eq!(completed.workflow.owner_thread_id,first.workflow.owner_thread_id);
        let task = &completed.workflow.tasks[0];
        let current = task.executions.last().context("current native execution missing")?;
        assert_eq!(task.executions.len(),1);
        assert_ne!(current.id,original.id);
        assert_eq!(current.task_revision,2);
        let reference = task.latest_archive.as_ref().context("native archive pointer missing")?;
        let bytes = crate::goal_artifacts::read(&fixture.root(&id),reference).await?;
        let material: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(material["execution"]["id"],original.id);
        assert_eq!(material["execution"]["thread"],serde_json::to_value(&link)?);
        let requests = fixture.backend.received_requests().await.context("native requests unavailable")?;
        let history = fixture.historical_runtime(&completed,task).await?;
        tokio::fs::write(fixture.controls.join("native-historical-evidence.json"),serde_json::to_vec_pretty(&history)?).await?;
        assert_eq!(history["historical_only"],true);
        assert_eq!(history["current_artifact_authority"],false);
        let entry = history["entries"].as_array().into_iter().flatten().find(|entry| entry["execution_id"] == original.id)
            .with_context(|| format!("original native history missing: {history}"))?;
        assert_eq!(entry["executor"],"built_in");
        assert_eq!(entry["thread_id"],link.thread_id);
        assert_eq!(entry["turn_id"],link.turn_id);
        assert_eq!(entry["current_task_version"],false);
        assert_eq!(entry["timeline_complete"],false);
        assert_eq!(entry["public_history_scan_complete"],false);
        assert!(entry["omitted_observations"].as_u64().is_some_and(|count| count > 0));
        assert!(entry["observations"].as_array().is_some_and(Vec::is_empty));
        assert!(serde_json::to_vec(&history)?.len() <= 48 * 1024);
        assert!(entry["model_work"]["entries"].as_array().is_some_and(|work| work.iter().any(|work|
            work["kind"] == "provider_attempt" && work["observation"]["actual_model"] == "test-model")));
        let target = bitrouter_orchestrator::thread::ThreadTarget { thread_id:link.thread_id.clone(),
            server_instance_id:fixture.threads.capabilities().server_instance_id };
        let before = fixture.threads.read_stored_thread_view(&target,&CallerContext::local()).await?.thread.cursor;
        let inspected = crate::goal_code::read_archive_fixture(&fixture.socket,&fixture.root(&id),&id,"inspect",reference).await?;
        assert_eq!(inspected.title,format!("Archived execution {}",original.id));
        assert_eq!(inspected.status,"historical · read only");
        assert!(inspected.details.iter().any(|text| text.contains(&link.turn_id)));
        let mut changed = bytes.clone();
        let first_byte = changed.first_mut().context("empty archive")?;
        *first_byte ^= 1;
        tokio::fs::write(&reference.path,changed).await?;
        let refused = crate::goal_code::read_archive_fixture(&fixture.socket,&fixture.root(&id),&id,"inspect",reference).await;
        tokio::fs::write(&reference.path,&bytes).await?;
        assert!(refused.is_err(),"tampered archive opened as verified history");
        assert_eq!(fixture.threads.read_stored_thread_view(&target,&CallerContext::local()).await?.thread.cursor,before);
        assert_eq!(fixture.backend.received_requests().await.context("native requests unavailable")?.len(),requests.len());
        assert_eq!(tokio::fs::read_to_string(fixture.workspace.join("seed.txt")).await?,"original input\n");
        Ok(())
    }.await;
    fixture.finish(result).await
}
