//! Credentialed runtime input evaluation. Workflow state is a labeled domain fixture.
use super::*;
use bitrouter_orchestrator::goal::{GoalOperation, GoalStore, OperationKind};
mod corpus;
mod sequences;
use corpus::{bootstrap_input_state, effect_violations, model_evidence, seed_state};

async fn keep_owner(coordinator: &Coordinator, goal_id: &str) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        coordinator.renew_owner(goal_id).await?;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicitly supplied OpenRouter test key and private output directory"]
async fn live_continuous_goal_inputs_use_persisted_state_and_durable_effects() -> Result<()> {
    sequences::evaluate_continuous().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicitly supplied OpenRouter test key and private output directory"]
async fn live_input_corpus_preserves_scope_permissions_and_durable_feedback() -> Result<()> {
    evaluate_inputs(corpus_cases()?).await
}

fn corpus_cases() -> Result<Vec<serde_json::Value>> {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/evidence/bro-goal-workflow/input-eval-cases.jsonl"
    ))
    .lines()
    .map(|line| Ok(serde_json::from_str(line)?))
    .collect()
}

async fn evaluate_inputs(cases: Vec<serde_json::Value>) -> Result<()> {
    use sea_orm::ConnectionTrait as _;
    let output = PathBuf::from(std::env::var("BRO_GOAL_LIVE_EVAL_OUTPUT")?);
    let credential = std::env::var("OPENROUTER_API_KEY")?;
    anyhow::ensure!(
        !credential.trim().is_empty(),
        "live evaluation credential is absent"
    );
    tokio::fs::create_dir_all(&output).await?;
    let output = tokio::fs::canonicalize(output).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let workspace = output.join("workspace");
    tokio::fs::create_dir_all(&workspace).await?;
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for definition in &cases {
        let case = definition["id"].as_str().context("case id missing")?;
        let messages = match definition
            .get("messages")
            .and_then(|messages| messages.as_array())
        {
            Some(messages) => messages.clone(),
            None => vec![definition.clone()],
        };
        anyhow::ensure!(!messages.is_empty(), "evaluation messages are empty");
        let state = definition["state"].as_str().context("case state missing")?;
        let seed_db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&seed_db).await?;
        let seed_store = Arc::new(crate::goal_store::DatabaseGoalStore::new(seed_db));
        let seed_service = GoalService::new(seed_store.clone());
        let GoalReply::Accepted { operation: creation } = seed_service.execute("local",GoalCommand::Create {
            operation_id:format!("live-seed-{case}"), title:"Evaluate current Goal input".into(),
            workspace:workspace.clone(), objective:"Reproduce the parser issue and deliver checked positive and negative-input results. Preserve independent branches.".into(),
            budget_limits:bitrouter_orchestrator::goal::budget::Limits {
                elapsed_ms:None, managed_token_estimates_micro_usd:Some(6_000_000),
            },
        },now()?).await? else { anyhow::bail!("live fixture creation has no receipt") };
        let mut seed = seed_store.get("local", &creation.goal_id).await?;
        let shared_workspaces = output.join(format!("{case}-shared-workspaces"));
        seed_state(&mut seed, state, &shared_workspaces).await?;
        if let Some(context) = definition.get("fixture_context") {
            let task = seed
                .workflow
                .tasks
                .iter_mut()
                .find(|task| task.spec.id == "left")
                .context("sequence context has no left task")?;
            task.spec
                .instruction
                .push_str(&format!(" Fictional supplied task facts: {}", context));
        }
        let seed_bytes = serde_json::to_vec(&seed)?;
        tokio::fs::write(
            output.join(format!("{case}-shared-domain-snapshot.json")),
            &seed_bytes,
        )
        .await?;
        let mut shared_trusted_state: Option<serde_json::Value> = None;
        for typed in [false, true] {
            let lane = if typed {
                "typed-upgrade"
            } else {
                "rules-reasoning"
            };
            let home = output.join(format!("{case}-{lane}"));
            tokio::fs::create_dir_all(&home).await?;
            let config_path = home.join("bitrouter.yaml");
            let fault_backend = sequences::inject_backend(definition).await?;
            let typed_api = fault_backend
                .as_ref()
                .map(|fault| fault.server.uri())
                .unwrap_or_else(|| "https://openrouter.ai/api".into());
            let reasoning_timeout = fault_backend
                .as_ref()
                .is_some_and(|fault| fault.reasoning_timeout);
            let reasoning_api = if reasoning_timeout {
                format!("{}/v1", typed_api)
            } else {
                "https://openrouter.ai/api/v1".into()
            };
            let decision = if typed {
                format!(
                    r#"
decision_model:
  model: typesafe/jev-1.13
  base_url: {typed_api}
  api_key_env: OPENROUTER_API_KEY
  timeout_ms: {}
  pricing: {{ input_usd_per_million: 0.042, output_usd_per_million: 0, max_input_tokens: 32000 }}
"#,
                    if fault_backend.is_some() { 100 } else { 30_000 }
                )
            } else {
                String::new()
            };
            let configuration = format!(
                r#"
inherit_defaults: false
server: {{ listen: "127.0.0.1:0", skip_auth: true }}
registry: {{ enabled: false }}
database: {{ url: "sqlite://{}?mode=rwc" }}
providers:
  live-openrouter:
    api_base: {reasoning_api}
    timeouts: {{ total_secs: {} }}
    api_key: "${{OPENROUTER_API_KEY}}"
    api_protocol: [{{'*': chat_completions}}]
    models:
      - id: openai/gpt-6.1-sol
        token_limits: {{ max_input_tokens: 1050000, max_output_tokens: 128000, context_window: 1050000 }}
        pricing:
          input_micro_usd_per_token: 2
          output_micro_usd_per_token: 10
          cache_read_micro_usd_per_token: 0.1
          cache_write_micro_usd_per_token: 2.5
          context_tiers:
            - above_input_tokens: 272000
              input_micro_usd_per_token: 4
              output_micro_usd_per_token: 15
              cache_read_micro_usd_per_token: 0.2
              cache_write_micro_usd_per_token: 5
models:
  reasoning:
    endpoints: [{{ provider: live-openrouter, service_id: openai/gpt-6.1-sol }}]
chat: {{ model: reasoning }}
{}
"#,
                home.join("evaluation.db").display(),
                if reasoning_timeout { 1 } else { 180 },
                decision
            );
            tokio::fs::write(&config_path, configuration).await?;
            let source = crate::paths::ConfigSource::File(config_path.clone());
            let config = crate::paths::load_config(&source).await?;
            let assembled =
                crate::assemble::build_app_with_path(&config, Some(&config_path)).await?;
            let store = Arc::new(crate::goal_store::DatabaseGoalStore::new(
                assembled.db.clone(),
            ));
            // Each lane receives the exact same persisted snapshot and creation receipt.
            store
                .commit(None, serde_json::from_slice(&seed_bytes)?, creation.clone())
                .await?;
            let goals = Arc::new(GoalService::new(store.clone()));
            let routing = Arc::new(ConfigRoutingTable::from_config(config));
            let supervisor = Supervisor::open(source, routing.clone()).await?;
            let SessionResponse::Authorized { grant } = supervisor
                .dispatch(SessionRequest::authorize(
                    "live-input-eval".into(),
                    BTreeSet::from([
                        SessionScope::Start,
                        SessionScope::Peek,
                        SessionScope::Transcript,
                        SessionScope::Respond,
                        SessionScope::Stop,
                    ]),
                ))
                .await?
            else {
                anyhow::bail!("input evaluation owner authorization missing")
            };
            let coordinator = Arc::new(Coordinator {
                id: "live-input-eval".into(),
                app: Arc::new(assembled.app),
                goals: goals.clone(),
                supervisor,
                routing,
                workspaces: shared_workspaces.clone(),
                home: home.clone(),
                grant,
                workspace_gate: tokio::sync::Mutex::new(()),
                stop_gate: tokio::sync::Mutex::new(()),
                gate: crate::daemon_handoff::HandoffGate::default(),
            });
            coordinator.claim_owner(&seed).await?;
            bootstrap_input_state(&goals, &coordinator.id, &seed.id, state).await?;
            for (step_index, message) in messages.iter().enumerate() {
                let step_case = if definition.get("messages").is_some() {
                    format!("{case}-step-{}", step_index + 1)
                } else {
                    case.to_string()
                };
                let sequence_id = definition.get("messages").is_some().then_some(case);
                let mut step_definition = message.clone();
                step_definition["id"] = step_case.clone().into();
                step_definition["state"] = state.into();
                let definition = &step_definition;
                let case = step_case.as_str();
                let text = definition["input"].as_str().context("case input missing")?;
                let current = store.get("local", &seed.id).await?;
                let before = current.clone();
                let withdrawal = bitrouter_orchestrator::goal::input::withdrawal_reference(text)?;
                let command = if let Some(id) = withdrawal {
                    let source = current
                        .inputs
                        .unresolved_inputs()
                        .into_iter()
                        .find(|input| input.input_id == id)
                        .context("withdrawal seed missing")?;
                    GoalCommand::WithdrawInput {
                        operation_id: format!("live-input-{case}"),
                        goal_id: seed.id.clone(),
                        expected_revision: current.revision,
                        input_id: id.into(),
                        source_requirements_revision: source.source_requirements_revision,
                    }
                } else {
                    GoalCommand::Input {
                        operation_id: format!("live-input-{case}"),
                        goal_id: seed.id.clone(),
                        expected_revision: current.revision,
                        text: text.into(),
                        task_reference: text
                            .strip_prefix("/task ")
                            .and_then(|rest| rest.split_whitespace().next())
                            .map(str::to_owned),
                    }
                };
                goals.execute("local", command, now()?).await?;
                let admitted = store.get("local", &seed.id).await?;
                // Reuse production owner renewal without changing its lease bound.
                let input = async {
                    if withdrawal.is_none() {
                        coordinator.drive_input(&admitted).await
                    } else {
                        Ok(())
                    }
                };
                let renewal = keep_owner(&coordinator, &seed.id);
                tokio::pin!(input, renewal);
                let drive_error = tokio::select! {
                    result = &mut input => result.err().map(|error|error.to_string()),
                    result = &mut renewal => { result?; anyhow::bail!("live input owner renewal unexpectedly ended"); },
                };
                let after = store.get("local", &seed.id).await?;
                let mut operations = Vec::<GoalOperation>::new();
                let mut cursor = 0;
                loop {
                    let page = store
                        .history("local", &seed.id, cursor, Some(after.revision), 16)
                        .await?;
                    operations.extend(page.operations);
                    let Some(next) = page.next_after else { break };
                    cursor = next;
                }
                let observed = after.inputs.latest.as_ref();
                let classification =
                    match observed.and_then(|result| result.classification.as_ref()) {
                        Some(reference) => Some(serde_json::from_slice::<serde_json::Value>(
                            &crate::goal_artifacts::read(
                                &crate::goal_artifacts::root(&coordinator.workspaces, &seed.id),
                                reference,
                            )
                            .await?,
                        )?),
                        None => None,
                    };
                let feedback = crate::goal_code::input_feedback_fixture(&after);
                let violations = effect_violations(definition, &before, &after, &feedback)?;
                failures.extend(
                    violations
                        .iter()
                        .map(|reason| format!("{case}/{lane}: {reason}")),
                );
                let new_operations = operations
                    .iter()
                    .filter(|operation| operation.revision > before.revision)
                    .cloned()
                    .collect::<Vec<_>>();
                let model_reports = model_evidence(&coordinator, &seed.id, &new_operations).await?;
                let trusted_state = model_reports
                    .iter()
                    .find_map(|record| record.get("plan"))
                    .map(|plan| -> Result<serde_json::Value> {
                        let text = plan["prompt"]["messages"][0]["content"][0]["text"]
                            .as_str()
                            .context("input model state text absent")?;
                        let payload: serde_json::Value = serde_json::from_str(text)?;
                        payload
                            .get("trusted_state")
                            .cloned()
                            .context("input model trusted state absent")
                    })
                    .transpose()?;
                if let Some(state) = &trusted_state
                    && step_index == 0
                {
                    if let Some(shared) = &shared_trusted_state {
                        if state != shared {
                            failures.push(format!(
                                "{case}/{lane}: model lanes received different trusted state"
                            ));
                        }
                    } else {
                        shared_trusted_state = Some(state.clone());
                    }
                }
                results.push(serde_json::json!({"case":case,"sequence":sequence_id,"step":step_index + 1,"trajectory":"later steps consume this lane persisted state, not a reset shared snapshot","lane":lane,"input":text,"annotation_status":definition.get("annotation_review").cloned().unwrap_or_else(||serde_json::json!("codex_proposed_unreviewed")),
                "before":before,"after":after,"classification":classification,"operations":operations,"drive_error":drive_error,"feedback":feedback,"effect_violations":violations,"model_reports":model_reports,"harness_effects":"not_exercised_domain_fixture_only"}));
                tokio::fs::write(
                    output.join("runtime-input-results.json"),
                    serde_json::to_vec_pretty(&results)?,
                )
                .await?;
                for (matches, diagnostic) in [
                    (drive_error.is_none(), "runtime rejected input processing"),
                    (
                        withdrawal.is_some()
                            || reasoning_timeout
                            || observed.is_some_and(|result| result.reasoning_request_id.is_some()),
                        "no real reasoning request identity",
                    ),
                    (
                        withdrawal.is_some()
                            || observed
                                .is_some_and(|result| result.classification.is_some() == typed),
                        "typed evidence does not match lane",
                    ),
                    (
                        withdrawal.is_some()
                            || !typed
                            || fault_backend.is_some()
                            || classification
                                .as_ref()
                                .is_some_and(|value| value["outcome"]["Ok"].is_object()),
                        "typed backend did not return a valid native outcome",
                    ),
                ] {
                    if !matches {
                        failures.push(format!("{case}/{lane}: {diagnostic}"));
                    }
                }
                if let Some(fault) = &fault_backend {
                    failures.extend(
                        sequences::fault_violations(
                            &step_definition,
                            typed,
                            classification.as_ref(),
                            &before,
                            &after,
                            &model_reports,
                            &fault.server,
                        )
                        .await?
                        .into_iter()
                        .map(|violation| format!("{case}/{lane}: {violation}")),
                    );
                }
                let count = assembled
                    .db
                    .query_one(sea_orm::Statement::from_string(
                        sea_orm::DatabaseBackend::Sqlite,
                        "SELECT COUNT(*) AS count FROM bro_managed_sessions".to_owned(),
                    ))
                    .await?
                    .context("session count missing")?;
                anyhow::ensure!(
                    count.try_get::<i64>("", "count")? == 0,
                    "input evaluation unexpectedly started a harness"
                );
                anyhow::ensure!(withdrawal.is_some() || operations.iter().any(|operation|operation.id == format!("live-input-{case}") && matches!(&operation.kind,OperationKind::Input {text:original,..} if original == text)),"original user input was not persisted");
            }
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "live input evaluation failed: {}",
        failures.join("; ")
    );
    eprintln!(
        "{} live cases completed in both lanes; domain workflow fixtures, no harness-adoption or calibrated-accuracy claim. Evidence: {}",
        cases.len(),
        output.display()
    );
    Ok(())
}
