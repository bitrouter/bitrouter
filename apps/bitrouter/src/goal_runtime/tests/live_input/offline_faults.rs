//! Deterministic transport contracts, independent of credentialed semantic evals.
use super::*;
use sea_orm::ConnectionTrait as _;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_http_faults_upgrade_to_local_reasoning_and_persist_feedback() -> Result<()> {
    for fault in ["timeout", "unavailable", "contradictory_distribution"] {
        input_fault(fault, false).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_reasoning_http_outcome_preserves_request_and_budget_without_effects() -> Result<()>
{
    input_fault("unavailable", true).await
}

async fn input_fault(typed_fault: &str, reasoning_timeout: bool) -> Result<()> {
    let text = "What is the current Goal progress?";
    let definition = serde_json::json!({"input":text,"backend_fault":{
        "typed":typed_fault,"reasoning":if reasoning_timeout { "timeout" } else { "fixed_local_response" }
    }});
    let backend = sequences::inject_backend(&definition)
        .await?
        .context("fault server missing")?;
    if !reasoning_timeout {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(provider::model)
            .mount(&backend.server)
            .await;
    }
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    let workspace = home.join("workspace");
    tokio::fs::create_dir_all(&workspace).await?;
    let workspaces = home.join("workspaces");
    let config_path = home.join("bitrouter.yaml");
    tokio::fs::write(
        &config_path,
        format!(
            r#"
inherit_defaults: false
registry: {{ enabled: false }}
server: {{ listen: "127.0.0.1:0", skip_auth: true }}
database: {{ url: "sqlite://{}?mode=rwc" }}
providers:
  fixture:
    api_base: {}/v1
    api_key: offline-fixture-key
    timeouts: {{ total_secs: {} }}
    api_protocol: [{{'*': chat_completions}}]
    models:
      - id: test-model
        token_limits: {{ max_input_tokens: 32000, max_output_tokens: 8192, context_window: 48000 }}
        pricing: {{ input_micro_usd_per_token: 1, output_micro_usd_per_token: 1, cache_read_micro_usd_per_token: 1, cache_write_micro_usd_per_token: 1 }}
models:
  reasoning:
    endpoints: [{{ provider: fixture, service_id: test-model }}]
chat: {{ model: reasoning }}
decision_model:
  model: offline-typed
  base_url: {}
  api_key_env: USER
  timeout_ms: 100
  pricing: {{ input_usd_per_million: 0.042, output_usd_per_million: 0, max_input_tokens: 32000 }}
"#,
            home.join("fault.db").display(),
            backend.server.uri(),
            if reasoning_timeout { 1 } else { 10 },
            backend.server.uri()
        ),
    )
    .await?;
    let seed_db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&seed_db).await?;
    let store = Arc::new(crate::goal_store::DatabaseGoalStore::new(seed_db));
    let goals = Arc::new(GoalService::new(store.clone()));
    let GoalReply::Accepted {
        operation: creation,
    } = goals
        .execute(
            "local",
            GoalCommand::Create {
                operation_id: "offline-fault-goal".into(),
                title: "Offline input transport".into(),
                workspace,
                objective: "Deliver checked parser results while preserving independent work"
                    .into(),
                budget_limits: bitrouter_orchestrator::goal::budget::Limits {
                    elapsed_ms: None,
                    managed_token_estimates_micro_usd: Some(6_000_000),
                },
            },
            now()?,
        )
        .await?
    else {
        anyhow::bail!("creation receipt missing")
    };
    // This labeled state models work already in progress; no harness is launched.
    let mut seed = store.get("local", &creation.goal_id).await?;
    seed_state(&mut seed, "open_change_question", &workspaces).await?;
    let (coordinator, store, db) = input_coordinator(
        "offline-input-fault",
        &config_path,
        workspaces,
        seed.clone(),
        creation,
    )
    .await?;
    let goals = coordinator.goals.clone();
    bootstrap_input_state(&goals, &coordinator.id, &seed.id, "open_change_question").await?;
    let before = store.get("local", &seed.id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Input {
                operation_id: "offline-fault-input".into(),
                goal_id: seed.id.clone(),
                expected_revision: before.revision,
                text: text.into(),
                task_reference: None,
            },
            now()?,
        )
        .await?;
    coordinator
        .drive_input(&store.get("local", &seed.id).await?)
        .await?;
    let after = store.get("local", &seed.id).await?;
    let reopened = crate::goal_store::DatabaseGoalStore::new(
        crate::db::connect(&format!(
            "sqlite://{}?mode=rwc",
            home.join("fault.db").display()
        ))
        .await?,
    );
    assert_eq!(reopened.get("local", &seed.id).await?, after);
    let mut operations = Vec::new();
    let mut cursor = before.revision;
    loop {
        let page = store
            .history("local", &seed.id, cursor, Some(after.revision), 16)
            .await?;
        operations.extend(page.operations);
        let Some(next) = page.next_after else { break };
        cursor = next;
    }
    let result = after
        .inputs
        .latest
        .as_ref()
        .context("input result missing")?;
    let classification: serde_json::Value = serde_json::from_slice(
        &crate::goal_artifacts::read(
            &crate::goal_artifacts::root(&coordinator.workspaces, &seed.id),
            result
                .classification
                .as_ref()
                .context("typed fault evidence missing")?,
        )
        .await?,
    )?;
    let reports = model_evidence(&coordinator, &seed.id, &operations).await?;
    let violations = sequences::fault_violations(
        &definition,
        true,
        Some(&classification),
        &before,
        &after,
        &reports,
        &backend.server,
    )
    .await?;
    assert!(
        violations.is_empty(),
        "{typed_fault}/{reasoning_timeout}: {violations:?}; runtime clarification: {:?}",
        result.clarification
    );
    assert_eq!(after.requirements_revision, before.requirements_revision);
    assert_eq!(after.run_intent, before.run_intent);
    assert_eq!(after.workflow.tasks, before.workflow.tasks);
    assert_eq!(after.workflow.decisions, before.workflow.decisions);
    assert_eq!(after.inputs.unresolved_inputs()[0].input_id, "u1");
    assert!(
        !result.requirements_saved
            && result.control_saved.is_none()
            && result.permission_response_saved.is_none()
    );
    assert!(operations.iter().any(|operation| matches!(&operation.kind,
        OperationKind::Input { text: original, .. } if original == text)));
    let requests = backend
        .server
        .received_requests()
        .await
        .context("requests missing")?;
    let reasoning = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(
        reasoning.len(),
        1,
        "reasoning must run once; runtime clarification: {:?}; budget halt: {:?}",
        result.clarification,
        after.budget.halt
    );
    assert_eq!(
        provider::model_state(reasoning[0]).context("reasoning state missing")?["trusted_state"]["input"],
        text
    );
    use bitrouter_orchestrator::goal::accounting::WorkKind;
    assert_eq!(
        after.accounting.provider_attempts,
        before.accounting.provider_attempts + 1
    );
    let outcome = after
        .accounting
        .recent
        .iter()
        .find(|outcome| outcome.work.kind == WorkKind::ProviderAttempt)
        .context("reasoning accounting outcome missing")?;
    assert_eq!(
        after
            .budget
            .held
            .iter()
            .any(|held| held.work_id == outcome.work.id),
        reasoning_timeout,
        "reasoning's own reservation must remain held exactly when its billed outcome is unknown"
    );
    assert_eq!(
        outcome.estimated_model_tokens_micro_usd.is_none(),
        reasoning_timeout
    );
    if typed_fault == "timeout" {
        use bitrouter_orchestrator::goal::accounting::Event as AccountingEvent;
        // Typed work can age out of the bounded recent list during reasoning;
        // the immutable operation history remains its durable authority.
        let typed = operations
            .iter()
            .find_map(|operation| {
                if let OperationKind::Workflow { change } = &operation.kind
                    && let Change::Accounting {
                        event: AccountingEvent::Begin { work },
                    } = change.as_ref()
                    && work.kind == WorkKind::TypedDecision
                {
                    Some(work)
                } else {
                    None
                }
            })
            .context("typed accounting intent missing")?;
        assert!(operations.iter().any(|operation| matches!(&operation.kind,
            OperationKind::Workflow { change } if matches!(change.as_ref(),
                Change::Accounting { event: AccountingEvent::Observe {
                    id, estimated_model_tokens_micro_usd: None, ..
                } } if id == &typed.id))));
        assert!(
            after
                .budget
                .held
                .iter()
                .any(|held| held.work_id == typed.id)
        );
    }
    let feedback = crate::goal_code::input_feedback_fixture(&after);
    if reasoning_timeout {
        assert!(
            feedback
                .iter()
                .any(|line| line.contains("Needs clarification / unavailable"))
        );
    } else {
        assert_eq!(
            result.answer.as_deref(),
            Some("Fixture explanation: the two actual branches are still active")
        );
        assert!(result.reasoning_request_id.is_some());
        assert!(
            feedback
                .iter()
                .any(|line| line.contains("BRO model answer:"))
        );
    }
    let count = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM bro_managed_sessions".to_owned(),
        ))
        .await?
        .context("session count missing")?;
    assert_eq!(count.try_get::<i64>("", "count")?, 0);
    Ok(())
}
