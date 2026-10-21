//! Actual product CLI -> daemon -> independent Rust MCP peer -> model HTTP.

use super::*;
use sha2::{Digest, Sha256};

const ROOT: &str = "skill://review/SKILL.md";
const SUPPORT: &str = "skill://review/references/check.md";
const MARKER: &str = "MCP_CONSENTED_CLI_BODY";

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn requests(journal: &std::path::Path) -> Result<Vec<Value>> {
    match mcp_fixture::requests(journal) {
        Ok(requests) => Ok(requests),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

async fn fixture() -> Result<(Fixture, MockServer, PathBuf)> {
    fixture_at(None).await
}

#[tokio::test]
async fn managed_cli_child_inherits_required_selector_but_waits_for_its_own_skill_consent()
-> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    let roots = Arc::new(AtomicUsize::new(0));
    let children = Arc::new(AtomicUsize::new(0));
    let root_count = roots.clone();
    let child_count = children.clone();
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(move|request:&wiremock::Request|{
            let Ok(value)=serde_json::from_slice::<Value>(&request.body) else{return ResponseTemplate::new(400);};
            let child=value["messages"].as_array().is_some_and(|messages|messages.iter().any(|message|message["role"]=="user"&&message["content"]=="child-work"));
            let result=if child {child_count.fetch_add(1,Ordering::SeqCst);text_result("child completed")}
                else if root_count.fetch_add(1,Ordering::SeqCst)==0 {
                    json!({"id":"spawn-child","object":"chat.completion","model":"model","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"spawn-required-child","type":"function","function":{"name":"spawn_agent","arguments":json!({"task":{"text":"child-work","fresh_context":true}}).to_string()}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}})
                } else {text_result("parent completed")};
            ResponseTemplate::new(200).set_body_json(result)
        }).with_priority(1).mount(&upstream).await;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let task = fixture
        .coding_command("required-child-input")
        .args(["--skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let parent = pending(&fixture, "required-child-input").await?;
    let parent_request = parent
        .skill_consents
        .last()
        .context("parent consent")?
        .clone();
    ensure!(
        answer(
            &fixture,
            &parent,
            &client.daemon_instance_id,
            "required-child-input",
            "allow"
        )
        .await?
        .status
        .success()
    );
    let mut child = pending(&fixture, "required-child-input").await?;
    child
        .skill_consents
        .retain(|request| request.consent.approved.is_none() && !request.consent.revoked);
    let child_request = child
        .skill_consents
        .last()
        .context("child consent")?
        .clone();
    ensure!(
        child_request.agent_turn_id != parent_request.agent_turn_id
            && child_request.consent.request_id != parent_request.consent.request_id
    );
    ensure!(children.load(Ordering::SeqCst) == 0);
    ensure!(
        requests(&journal)?
            .iter()
            .filter(|request| request["method"] == "resources/read")
            .count()
            == 1
    );
    // A committed parent answer may be replayed, but never grants this child.
    ensure!(
        answer(
            &fixture,
            &parent,
            &client.daemon_instance_id,
            "required-child-input",
            "allow"
        )
        .await?
        .status
        .success()
    );
    ensure!(children.load(Ordering::SeqCst) == 0);
    ensure!(
        answer(
            &fixture,
            &child,
            &client.daemon_instance_id,
            "required-child-input",
            "allow"
        )
        .await?
        .status
        .success()
    );
    let output = match tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await
    {
        Ok(output) => output?,
        Err(error) => {
            let report = client
                .request(Operation::Status {
                    name: "continued".into(),
                    operation_id: Some("required-child-input".into()),
                })
                .await;
            let snapshot = fixture
                .stored()
                .await?
                .snapshot
                .context("stalled checkpoint")?;
            let agents = snapshot.agents.values().map(|agent| {
                json!({"agent":agent.agent_id,"turn":agent.turn.as_ref().map(|turn|json!({"status":turn.status,"answer":turn.final_answer,"step":turn.steps.last().map(|step|json!({"id":step.step_id,"preparation":step.preparation_work.last()}))}))})
            }).collect::<Vec<_>>();
            let log = std::fs::read(fixture.home.path().join("daemon.log"))?;
            let tail = String::from_utf8_lossy(&log[log.len().saturating_sub(16_384)..]);
            anyhow::bail!(
                "approved parent/child CLI did not finish: {error}; roots={} children={} report={report:?} agents={} daemon log={tail}",
                roots.load(Ordering::SeqCst),
                children.load(Ordering::SeqCst),
                serde_json::to_string(&agents)?
            );
        }
    };
    let snapshot = fixture
        .stored()
        .await?
        .snapshot
        .context("child checkpoint")?;
    ensure!(output.status.success(),"agents={} stderr={}",serde_json::to_string(&snapshot.agents.values().map(|agent|json!({"id":agent.agent_id,"turn":agent.turn.as_ref().map(|turn|json!({"status":turn.status,"reason":turn.terminal_reason,"diagnostic":turn.steps.last().map(|step|&step.failure_diagnostic),"routes":turn.steps.last().and_then(|step|step.decision.as_ref()).map(|decision|&decision.routes)}))})).collect::<Vec<_>>())?,String::from_utf8_lossy(&output.stderr));
    lines(&output)?;
    let settled = wait_owner_finished(&client, "required-child-input").await?;
    ensure!(
        settled.released
            && settled.run.as_ref().is_some_and(
                |run| run.status == bitrouter_orchestrator::core::session::RunStatus::Completed
            )
    );
    ensure!(children.load(Ordering::SeqCst) == 1);
    ensure!(
        requests(&journal)?
            .iter()
            .filter(|request| request["method"] == "resources/read")
            .count()
            == 2
    );
    let http = upstream
        .received_requests()
        .await
        .context("model requests")?;
    let models = http
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .map(|request| serde_json::from_slice::<Value>(&request.body))
        .collect::<Result<Vec<_>, _>>()?;
    let child_model = models
        .iter()
        .find(|model| {
            model["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message["content"] == "child-work")
            })
        })
        .context("actual child model")?;
    let messages = child_model["messages"]
        .as_array()
        .context("child messages")?;
    ensure!(messages.iter().any(|message| {
        message["role"] == "user"
            && message.to_string().contains(MARKER)
            && message
                .to_string()
                .contains(&child_request.source.entry_digest)
    }));
    ensure!(
        messages
            .iter()
            .filter(|message| message["role"] == "system")
            .all(|message| !message.to_string().contains(MARKER))
    );
    fixture.stop().await?;
    let cold = explain(
        &fixture,
        &[
            "--managed-session",
            "continued",
            "--request-id",
            "required-child-input",
        ],
        false,
    )
    .await?;
    ensure!(cold.status.success());
    let cold: Value = serde_json::from_slice(&cold.stdout)?;
    let work = cold["explanation"]["work_units"]
        .as_array()
        .context("root/child facts")?;
    ensure!(
        work.len() == 2
            && work
                .iter()
                .all(
                    |work| work["selection"]["constraints"]["include"][0]["uri"] == ROOT
                        && work["selection"]["candidates"][0]["consents"][0]["approved"] == true
                )
    );
    Ok(())
}

#[tokio::test]
async fn managed_cli_reads_bound_supporting_files_into_verified_model_data_and_cold_explain()
-> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            if counter.fetch_add(1, Ordering::SeqCst) > 0 {
                return ResponseTemplate::new(200).set_body_json(text_result("supporting file used"));
            }
            let Ok(request) = serde_json::from_slice::<Value>(&request.body) else { return ResponseTemplate::new(400); };
            let Some(skill_id) = request["tools"].as_array().and_then(|tools| tools.iter().find(|tool| tool["function"]["name"] == "skill_read")).and_then(|tool| tool["function"]["parameters"]["properties"]["skill_id"]["enum"][0].as_str()) else { return ResponseTemplate::new(400).set_body_string("skill_read declaration absent"); };
            ResponseTemplate::new(200).set_body_json(json!({"id":"support-read","object":"chat.completion","model":"model",
                "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"bound-support","type":"function","function":{"name":"skill_read","arguments":json!({"skill_id":skill_id,"reference":"references/check.md"}).to_string()}}]},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}))
        }).with_priority(1).mount(&upstream).await;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let task = fixture
        .coding_command("supporting-input")
        .args(["--skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let report = pending(&fixture, "supporting-input").await?;
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    ensure!(
        answer(
            &fixture,
            &report,
            &client.daemon_instance_id,
            "supporting-input",
            "allow"
        )
        .await?
        .status
        .success()
    );
    lines(&tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await??)?;
    let settled = wait_owner_finished(&client, "supporting-input").await?;
    ensure!(
        settled.released
            && settled.run.as_ref().is_some_and(
                |run| run.status == bitrouter_orchestrator::core::session::RunStatus::Completed
            )
    );
    ensure!(calls.load(Ordering::SeqCst) == 2);
    let http = upstream
        .received_requests()
        .await
        .context("model requests")?;
    let models = http
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .map(|request| serde_json::from_slice::<Value>(&request.body))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(models.len() == 2 && !models[0].to_string().contains("BEGIN VERIFIED FILE DATA"));
    let messages = models[1]["messages"]
        .as_array()
        .context("second model messages")?;
    ensure!(messages.iter().any(|message| message["role"] == "user"
        && message["content"].as_str().is_some_and(
            |text| text.contains("BEGIN VERIFIED FILE DATA")
                && text.ends_with("support\n")
                && text.contains(SUPPORT)
                && text.contains(&report.skill_consents[0].source.entry_digest)
        )));
    ensure!(
        messages
            .iter()
            .filter(|message| message["role"] != "user")
            .all(|message| !message.to_string().contains("BEGIN VERIFIED FILE DATA"))
    );
    let reads = requests(&journal)?
        .into_iter()
        .filter(|request| request["method"] == "resources/read")
        .collect::<Vec<_>>();
    ensure!(
        reads.len() == 2
            && reads[0]["params"]["uri"] == ROOT
            && reads[1]["params"]["uri"] == SUPPORT
    );
    let stored = fixture.stored().await?;
    let snapshot = stored.snapshot.context("original checkpoint")?;
    let selection = snapshot
        .skill_selection(&snapshot.root_turn().context("Turn")?.agent_turn_id)
        .context("selection")?;
    ensure!(
        selection.candidates[0]
            .files
            .get(SUPPORT)
            .map(String::as_str)
            == Some("support\n")
    );
    fixture.stop().await?;
    std::fs::remove_file(fixture.home.path().join("mcp-fixture.json"))?;
    let before = fixture.store().await?.load().await?;
    let mcp_before = requests(&journal)?.len();
    let loaded = explain(
        &fixture,
        &[
            "--managed-session",
            "continued",
            "--request-id",
            "supporting-input",
        ],
        false,
    )
    .await?;
    ensure!(
        loaded.status.success(),
        "{}",
        String::from_utf8_lossy(&loaded.stderr)
    );
    let loaded: Value = serde_json::from_slice(&loaded.stdout)?;
    let candidate = explained_candidate(&loaded)?;
    ensure!(
        candidate["supporting_files"][0]["reference"] == SUPPORT
            && candidate["supporting_files"][0]["bytes"] == 8
    );
    ensure!(
        candidate["prepared_model_requests"]
            .as_array()
            .context("model evidence")?
            .iter()
            .filter(|request| request["supporting_files"]
                .as_array()
                .is_some_and(|files| files.len() == 1))
            .count()
            == 1
    );
    ensure!(
        !loaded.to_string().contains("BEGIN VERIFIED FILE DATA")
            && !loaded.to_string().contains(MARKER)
    );
    ensure!(
        fixture.store().await?.load().await? == before && requests(&journal)?.len() == mcp_before
    );
    ensure!(
        bitrouter::daemon::probe_status(&fixture.socket)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn local_directory_files_keep_turn_binding_and_cold_evidence_after_directory_removal()
-> Result<()> {
    let (mut fixture, upstream, effect_calls) = effect_fixture().await?;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(|request: &wiremock::Request| !String::from_utf8_lossy(&request.body).contains("BEGIN VERIFIED FILE DATA"))
        .respond_with(move |request: &wiremock::Request| {
            let Ok(request) = serde_json::from_slice::<Value>(&request.body) else { return ResponseTemplate::new(400); };
            let Some(skill_id) = request["tools"].as_array().and_then(|tools| tools.iter().find(|tool| tool["function"]["name"] == "skill_read")).and_then(|tool| tool["function"]["parameters"]["properties"]["skill_id"]["enum"][0].as_str()) else { return ResponseTemplate::new(400); };
            ResponseTemplate::new(200).set_body_json(json!({"id":"local-support","object":"chat.completion","model":"model",
                "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"local-file","type":"function","function":{"name":"skill_read","arguments":json!({"skill_id":skill_id,"reference":"references/check.md"}).to_string()}}]},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}))
        }).with_priority(1).mount(&upstream).await;
    let source = fixture.workspace.join("skills/review");
    std::fs::create_dir_all(source.join("references"))?;
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: review\ndescription: review workflow\n---\nLOCAL_ROOT_V1\n",
    )?;
    std::fs::write(source.join("references/check.md"), "LOCAL_SUPPORT_V1\n")?;
    let installed = fixture
        .command()
        .args(["--json", "skills", "list"])
        .arg("--workspace")
        .arg(&fixture.workspace)
        .output()
        .await?;
    ensure!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed: Value = serde_json::from_slice(&installed.stdout)?;
    let identity = installed["skills"][0]["directory_id"]
        .as_str()
        .context("installation")?;
    let task = fixture
        .coding_command("local-file-input")
        .args(["--skill", &format!("project:{identity}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    wait_ready(&fixture).await?;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let active = fixture.inspect("local-file-input").await?;
    ensure!(active.resident && !active.released);
    let before = fixture
        .stored()
        .await?
        .snapshot
        .context("active checkpoint")?;
    let original_source = before
        .skill_selection(&before.root_turn().context("active Turn")?.agent_turn_id)
        .context("active selection")?
        .candidates[0]
        .source
        .clone();
    std::fs::write(fixture.home.path().join("effect-gate"), b"continue")?;
    let executed = tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await?;
    lines(&executed?)?;
    wait_owner_finished(&client, "local-file-input").await?;
    let stored = fixture.stored().await?;
    let snapshot = stored.snapshot.context("checkpoint")?;
    let selection = snapshot
        .skill_selection(&snapshot.root_turn().context("Turn")?.agent_turn_id)
        .context("selection")?;
    let original = &selection.candidates[0];
    ensure!(original.source == original_source);
    ensure!(
        original
            .files
            .get("references/check.md")
            .map(String::as_str)
            == Some("LOCAL_SUPPORT_V1\n")
    );
    let http = upstream
        .received_requests()
        .await
        .context("model requests")?;
    let models = http
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        models.len() == 3
            && effect_calls.load(Ordering::SeqCst) == 2
            && !models[0].to_string().contains("LOCAL_SUPPORT_V1")
    );
    ensure!(
        models[2].to_string().contains("LOCAL_ROOT_V1")
            && models[2].to_string().contains("LOCAL_SUPPORT_V1")
            && !models[2].to_string().contains("LOCAL_ROOT_V2")
            && !models[2].to_string().contains("LOCAL_SUPPORT_V2")
    );
    let messages = models[1]["messages"].as_array().context("messages")?;
    ensure!(messages.iter().any(|message| message["role"] == "user"
        && message.to_string().contains("LOCAL_SUPPORT_V1")
        && message.to_string().contains(identity)));
    ensure!(
        messages
            .iter()
            .filter(|message| message["role"] != "user")
            .all(|message| !message.to_string().contains("LOCAL_SUPPORT_V1"))
    );
    std::fs::remove_dir_all(&source)?;
    let missing = fixture
        .coding_command("after-local-uninstall")
        .args(["--read-only", "--skill", &format!("project:{identity}")])
        .output()
        .await?;
    ensure!(!missing.status.success());
    wait_owner_finished(&client, "after-local-uninstall").await?;
    ensure!(
        upstream
            .received_requests()
            .await
            .context("retained model requests")?
            .len()
            == 3
    );
    fixture.stop().await?;
    let report = explain(
        &fixture,
        &[
            "--managed-session",
            "continued",
            "--request-id",
            "local-file-input",
        ],
        false,
    )
    .await?;
    ensure!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report: Value = serde_json::from_slice(&report.stdout)?;
    let candidate = explained_candidate(&report)?;
    ensure!(candidate["source"] == serde_json::to_value(&original.source)?);
    ensure!(
        candidate["supporting_files"][0]["sha256"]
            == hex::encode(Sha256::digest(b"LOCAL_SUPPORT_V1\n"))
    );
    ensure!(
        candidate["prepared_model_requests"]
            .as_array()
            .context("requests")?
            .iter()
            .filter(|request| request["supporting_files"]
                .as_array()
                .is_some_and(|files| files.len() == 1))
            .count()
            == 2
    );
    ensure!(
        !report.to_string().contains("LOCAL_SUPPORT_V1")
            && !report.to_string().contains("LOCAL_SUPPORT_V2")
    );
    ensure!(
        bitrouter::daemon::probe_status(&fixture.socket)
            .await?
            .is_none()
    );
    Ok(())
}

async fn explain(
    fixture: &Fixture,
    selectors: &[&str],
    human: bool,
) -> Result<std::process::Output> {
    fixture
        .command()
        .env(
            "BITROUTER_TEST_HISTORY_DB",
            format!(
                "sqlite://{}?mode=rwc",
                fixture.home.path().join("state.db").display()
            ),
        )
        .arg(if human { "--human" } else { "--json" })
        .args(["skills", "explain"])
        .args(selectors)
        .arg("--config")
        .arg(&fixture.config)
        .output()
        .await
        .map_err(Into::into)
}

fn explained_candidate(report: &Value) -> Result<&Value> {
    report["explanation"]["work_units"]
        .as_array()
        .context("explained work")?
        .iter()
        .find_map(|work| work["selection"]["candidates"].as_array()?.first())
        .context("explained candidate")
}

#[tokio::test]
async fn managed_skill_explain_is_cold_after_shutdown_and_retains_only_original_evidence()
-> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let task = fixture
        .coding_command("explain-original")
        .args(["--skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pending = pending(&fixture, "explain-original").await?;
    let source = pending
        .skill_consents
        .last()
        .context("original consent")?
        .source
        .clone();
    let selectors = [
        "--managed-session",
        "continued",
        "--request-id",
        "explain-original",
    ];
    let selected = explain(&fixture, &selectors, false).await?;
    ensure!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let selected: Value = serde_json::from_slice(&selected.stdout)?;
    let candidate = explained_candidate(&selected)?;
    ensure!(candidate["selected"] == true && candidate["instructions"].is_null());
    ensure!(
        candidate["prepared_model_requests"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    let approved = answer(
        &fixture,
        &pending,
        &client.daemon_instance_id,
        "explain-original",
        "allow",
    )
    .await?;
    ensure!(approved.status.success());
    lines(&tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await??)?;
    wait_owner_finished(&client, "explain-original").await?;
    let executed = explain(&fixture, &selectors, false).await?;
    ensure!(executed.status.success());
    let executed: Value = serde_json::from_slice(&executed.stdout)?;
    let original_models = explained_candidate(&executed)?["prepared_model_requests"]
        .as_array()
        .context("original model evidence")?;
    ensure!(
        original_models.len() == 1
            && original_models[0]["attempts"][0]["model_result_received"] == true
    );
    ensure!(
        original_models[0]["attempts"][0]["provider_work"]
            .as_array()
            .context("HTTP execution evidence")?
            .iter()
            .any(|work| work["work"]["kind"] == "http_dispatch"
                && work["report"]["http_status"] == 200)
    );
    let original_request = original_models[0]["request_id"].clone();
    let next = fixture
        .coding_command("explain-newer")
        .args(["--exclude-skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .output()
        .await?;
    lines(&next)?;
    wait_owner_finished(&client, "explain-newer").await?;
    let http = upstream
        .received_requests()
        .await
        .context("actual model inputs")?;
    let model_inputs = http
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .collect::<Vec<_>>();
    ensure!(
        model_inputs.len() == 2 && !String::from_utf8_lossy(&model_inputs[1].body).contains(MARKER),
        "a later excluded Turn inherited old MCP instructions"
    );
    fixture.stop().await?;
    std::fs::remove_file(fixture.home.path().join("mcp-fixture.json"))?;
    std::fs::remove_dir_all(&fixture.workspace)?;
    let mut saved_config: Value = serde_json::from_slice(&std::fs::read(&fixture.config)?)?;
    saved_config["database"]["url"] = "${BITROUTER_TEST_HISTORY_DB}".into();
    saved_config["providers"] = "${MUST_NOT_RESOLVE_UNRELATED_PROVIDER_ENV}".into();
    saved_config["mcp_servers"] = "invalid-current-mcp-config".into();
    std::fs::write(&fixture.config, serde_json::to_vec(&saved_config)?)?;
    let before = fixture.store().await?.load().await?;
    let mcp_before = requests(&journal)?.len();
    let http_before = upstream
        .received_requests()
        .await
        .context("HTTP requests")?
        .len();
    let loaded = explain(&fixture, &selectors, false).await?;
    ensure!(
        loaded.status.success(),
        "{}",
        String::from_utf8_lossy(&loaded.stderr)
    );
    let loaded: Value = serde_json::from_slice(&loaded.stdout)?;
    ensure!(loaded["selector"]["kind"] == "managed_native");
    let candidate = explained_candidate(&loaded)?;
    ensure!(candidate["source"]["binding"] == serde_json::to_value(&source)?);
    ensure!(
        candidate["instructions"]["bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    let models = candidate["prepared_model_requests"]
        .as_array()
        .context("prepared request evidence")?;
    ensure!(
        models.len() == 1
            && models[0]["request_id"] == original_request
            && models[0]["attempts"].is_null(),
        "historical context must retain its original request without inventing retired attempt evidence"
    );
    ensure!(
        !loaded.to_string().contains(MARKER),
        "explain must not dump skill instructions"
    );
    let human = explain(&fixture, &selectors, true).await?;
    ensure!(
        human.status.success()
            && String::from_utf8_lossy(&human.stdout).contains("prepared model requests")
    );
    for wrong in [
        vec![
            "--managed-session",
            "continued",
            "--request-id",
            "unknown-input",
        ],
        vec!["--thread", "continued", "--turn", "explain-original"],
        vec![
            "--managed-session",
            "continued",
            "--request-id",
            "explain-original",
            "--agent-turn",
            "unrelated-turn",
        ],
    ] {
        ensure!(!explain(&fixture, &wrong, false).await?.status.success());
    }
    std::fs::remove_file(&fixture.config)?;
    let direct = fixture
        .command()
        .args([
            "skills",
            "explain",
            "--managed-session",
            "continued",
            "--request-id",
            "explain-original",
            "--database-url",
        ])
        .arg(format!(
            "sqlite://{}?mode=rwc",
            fixture.home.path().join("state.db").display()
        ))
        .output()
        .await?;
    ensure!(
        direct.status.success(),
        "{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    let direct: Value = serde_json::from_slice(&direct.stdout)?;
    ensure!(explained_candidate(&direct)?["source"] == candidate["source"]);
    let missing = fixture.home.path().join("never-create-history.db");
    let output = fixture
        .command()
        .args([
            "skills",
            "explain",
            "--managed-session",
            "continued",
            "--request-id",
            "explain-original",
            "--database-url",
        ])
        .arg(format!("sqlite://{}?mode=rwc", missing.display()))
        .output()
        .await?;
    ensure!(
        !output.status.success() && !missing.exists(),
        "cold inspection created a database"
    );
    ensure!(fixture.store().await?.load().await? == before);
    ensure!(
        requests(&journal)?.len() == mcp_before
            && upstream
                .received_requests()
                .await
                .context("HTTP requests")?
                .len()
                == http_before
    );
    ensure!(
        bitrouter::daemon::probe_status(&fixture.socket)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn cold_explain_of_a_lost_managed_owner_never_restores_or_reports_an_unobserved_result()
-> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(text_result("unobserved result"))
                .set_delay(Duration::from_secs(30)),
        )
        .with_priority(1)
        .mount(&upstream)
        .await;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let task = fixture
        .coding_command("lost-explain-input")
        .args(["--skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pending = pending(&fixture, "lost-explain-input").await?;
    let accepted = answer(
        &fixture,
        &pending,
        &client.daemon_instance_id,
        "lost-explain-input",
        "allow",
    )
    .await?;
    ensure!(accepted.status.success());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let stored = fixture.stored().await?;
            if stored
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.root_turn())
                .is_some_and(|turn| {
                    turn.steps.iter().any(|step| {
                        step.attempts
                            .iter()
                            .any(|attempt| !attempt.provider_work.is_empty())
                    })
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    fixture.crash().await?;
    let output = tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await??;
    ensure!(!output.status.success());
    let before = fixture.store().await?.load().await?;
    let mcp = requests(&journal)?.len();
    let report = explain(
        &fixture,
        &[
            "--managed-session",
            "continued",
            "--request-id",
            "lost-explain-input",
        ],
        false,
    )
    .await?;
    ensure!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report: Value = serde_json::from_slice(&report.stdout)?;
    ensure!(report["selector"]["released"] == false);
    let candidate = explained_candidate(&report)?;
    ensure!(candidate["instructions"].is_object());
    let model = candidate["prepared_model_requests"]
        .as_array()
        .and_then(|models| models.first())
        .context("original prepared model input")?;
    ensure!(
        model["attempts"][0]["report_received"] == false
            && model["attempts"][0]["model_result_received"] == false
    );
    ensure!(fixture.store().await?.load().await? == before && requests(&journal)?.len() == mcp);
    ensure!(
        bitrouter::daemon::probe_status(&fixture.socket)
            .await?
            .is_none()
    );
    Ok(())
}

async fn fixture_at(api: Option<std::net::SocketAddr>) -> Result<(Fixture, MockServer, PathBuf)> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/systemone")).respond_with(|request: &wiremock::Request| {
        let Ok(request) = serde_json::from_slice::<bitrouter_sdk::decision_model::types::DecisionRequest>(&request.body) else { return ResponseTemplate::new(400); };
        use bitrouter_sdk::decision_model::types::{Answer, Question};
        let mut answers = std::collections::BTreeMap::new();
        for (id, question) in request.questions {
            let answer = match question {
                Question::Noul { .. } => Answer::Noul { noul: 0.99 },
                Question::Choice { criteria, .. } if criteria.contains_key("full") => Answer::Choice {
                    choice: "full".into(), confidence: 1.0,
                    probabilities: criteria.keys().map(|key| (key.clone(), if key == "full" { 1.0 } else { 0.0 })).collect(),
                },
                _ => return ResponseTemplate::new(400),
            };
            answers.insert(id, answer);
        }
        ResponseTemplate::new(200).set_body_json(json!({"model":request.model,"answers":answers,"usage":{"input_tokens":17,"output_tokens":3}}))
    }).mount(&upstream).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let Ok(value) = serde_json::from_slice::<Value>(&request.body) else { return ResponseTemplate::new(400); };
            if value["stream"] == true {
                let content = json!({"id":"fixture","object":"chat.completion.chunk","created":1,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"review completed"},"finish_reason":null}]});
                let finish = json!({"id":"fixture","object":"chat.completion.chunk","created":1,"model":"model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
                ResponseTemplate::new(200).insert_header("Content-Type", "text/event-stream")
                    .set_body_string(format!("data: {content}\n\ndata: {finish}\n\ndata: [DONE]\n\n"))
            } else {
                ResponseTemplate::new(200).set_body_json(text_result("review completed"))
            }
        })
        .mount(&upstream)
        .await;
    let fixture = Fixture::configured(&upstream.uri(), |root, settings| {
        let body = format!("---\nname: review\ndescription: review workflow\nallowed-tools: shell\n---\n{MARKER}\n");
        let entry = json!({"uri":ROOT,"frontmatter":{"name":"review","description":"review workflow","allowed-tools":"shell"},
            "resources":[{"uri":ROOT,"digest":digest(body.as_bytes()),"size":body.len()},
                {"uri":SUPPORT,"digest":digest(b"support\n"),"size":8}],"extra":{"preserve":true}});
        let config = root.join("mcp-fixture.json");
        std::fs::write(&config, serde_json::to_vec(&json!({
            "capabilities":{"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}},
            "pages":{"first":{"resultType":"complete","skills":[entry.clone()],"ttlMs":0,"cacheScope":"private"}},
            "get":{ROOT:{"resultType":"complete","skill":entry,"ttlMs":0,"cacheScope":"private"}},
            "files":{ROOT:{"mimeType":"text/markdown","text":body},SUPPORT:{"mimeType":"text/markdown","text":"support\n"}}
        }))?)?;
        settings["mcp_servers"] = json!({"skills-peer":{"name":"skills-peer","transport":mcp_fixture::transport(&config,&root.join("mcp-requests.jsonl"))?}});
        settings["skills"] = json!({"mcp_sources":{"skills-peer":{"workspaces":[root.join("project")],"local":true}}});
        settings["decision_model"] = json!({"model":"workflow-fixture","base_url":upstream.uri(),"api_key_env":"BITROUTER_TEST_DECISION_KEY"});
        if let Some(address) = api {
            settings["agent_api"] = json!({"enabled":true,"listen":address.to_string(),"token_env":"BITROUTER_TEST_AGENT_TOKEN","workspaces":[root.join("project")]});
            settings["skills"]["mcp_sources"]["skills-peer"]["api_key_ids"] = json!(["agent-api"]);
        }
        Ok(())
    }).await?;
    let journal = fixture.home.path().join("mcp-requests.jsonl");
    Ok((fixture, upstream, journal))
}

async fn pending(fixture: &Fixture, key: &str) -> Result<Report> {
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let mut last = None;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let report = client
                .request(Operation::Status {
                    name: "continued".into(),
                    operation_id: Some(key.into()),
                })
                .await?;
            ensure!(
                report.resident || report.state == "not_found",
                "managed skill stopped before consent: {report:?}"
            );
            if report
                .skill_consents
                .iter()
                .any(|request| !request.consent.revoked && request.consent.approved.is_none())
            {
                return Ok(report);
            }
            last = Some(report);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("waiting for managed skill consent for {key}; last={last:?}"))?
}

async fn answer(
    fixture: &Fixture,
    report: &Report,
    boot: &str,
    key: &str,
    decision: &str,
) -> Result<std::process::Output> {
    let consent = report.skill_consents.last().context("skill consent")?;
    fixture
        .command()
        .args([
            "--json",
            "task",
            "managed-answer",
            "--session",
            "continued",
            "--request-id",
            key,
            "--server-instance",
            boot,
            "--consent-id",
            &consent.consent.request_id,
            "--decision",
            decision,
            "--config",
        ])
        .arg(&fixture.config)
        .output()
        .await
        .map_err(Into::into)
}

#[tokio::test]
async fn managed_cli_requires_unlisted_uri_and_keeps_constraints_in_original_input_identity()
-> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    let source_config = fixture.home.path().join("mcp-fixture.json");
    let mut source: Value = serde_json::from_slice(&std::fs::read(&source_config)?)?;
    source["pages"]["first"]["skills"] = json!([]);
    std::fs::write(&source_config, serde_json::to_vec(&source)?)?;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    ensure!(client.skill_constraints);
    let contradictory = fixture
        .coding_command("contradictory-skills")
        .args([
            "--skill",
            "mcp:skills-peer:skill://review/SKILL.md",
            "--exclude-skill",
            "mcp:skills-peer:skill://review/SKILL.md",
        ])
        .output()
        .await?;
    ensure!(!contradictory.status.success());
    ensure!(
        fixture
            .inspect("contradictory-skills")
            .await?
            .receipt
            .is_none()
    );
    ensure!(requests(&journal)?.is_empty());
    let task = fixture
        .coding_command("explicit-unlisted")
        .args(["--skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let report = pending(&fixture, "explicit-unlisted").await?;
    let before = requests(&journal)?;
    ensure!(
        before
            .iter()
            .filter(|request| request["method"] == "skills/get")
            .count()
            == 1
    );
    ensure!(
        before
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    let approved = answer(
        &fixture,
        &report,
        &client.daemon_instance_id,
        "explicit-unlisted",
        "allow",
    )
    .await?;
    ensure!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    lines(&tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await??)?;
    let stored = fixture.stored().await?;
    let snapshot = stored.snapshot.context("original checkpoint")?;
    let turn = snapshot.root_turn().context("original Turn")?;
    let selection = snapshot
        .skill_selection(&turn.agent_turn_id)
        .context("original selection")?;
    ensure!(selection.candidates.len() == 1 && selection.candidates[0].selected);
    ensure!(selection.candidates[0].reason == "explicitly_required");
    ensure!(turn.input.routing.skills.as_ref() == Some(&selection.constraints));
    let previous = requests(&journal)?.len();
    let changed = fixture
        .coding_command("explicit-unlisted")
        .args(["--exclude-skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .output()
        .await?;
    ensure!(
        !changed.status.success(),
        "different constraints reused an accepted key"
    );
    ensure!(requests(&journal)?.len() == previous);
    let http = upstream.received_requests().await.context("HTTP calls")?;
    ensure!(
        http.iter()
            .filter(|request| request.url.path() == "/chat/completions")
            .count()
            == 1
    );
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn managed_cli_exclusion_keeps_matching_mcp_content_out_of_the_model() -> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    let output = fixture
        .coding_command("exclude-skill")
        .args(["--exclude-skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .output()
        .await?;
    lines(&output)?;
    let stored = fixture.stored().await?;
    let snapshot = stored.snapshot.context("original checkpoint")?;
    let selection = snapshot
        .skill_selection(&snapshot.root_turn().context("original Turn")?.agent_turn_id)
        .context("original selection")?;
    ensure!(selection.candidates.len() == 1 && !selection.candidates[0].selected);
    ensure!(
        selection.candidates[0].reason == "explicitly_excluded"
            && selection.candidates[0].content.is_none()
    );
    ensure!(selection.candidates[0].consents.is_empty());
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read"
                && request["method"] != "skills/get")
    );
    let http = upstream.received_requests().await.context("HTTP calls")?;
    let models = http
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .collect::<Vec<_>>();
    ensure!(models.len() == 1 && !String::from_utf8_lossy(&models[0].body).contains(MARKER));
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn managed_cli_rejects_unavailable_required_uris_without_reads_or_models() -> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    for (key, selector) in [
        ("missing-uri", "mcp:skills-peer:skill://missing/SKILL.md"),
        ("missing-source", "mcp:unauthorized:skill://review/SKILL.md"),
    ] {
        let output = fixture
            .coding_command(key)
            .args(["--skill", selector])
            .output()
            .await?;
        ensure!(
            !output.status.success(),
            "required unavailable skill was silently skipped"
        );
        let settled = fixture.inspect(key).await?;
        ensure!(
            settled.receipt.is_some()
                && settled.released
                && settled
                    .run
                    .as_ref()
                    .is_some_and(|run| run.status
                        == bitrouter_orchestrator::core::session::RunStatus::Failed),
            "{settled:?}"
        );
    }
    let mcp = requests(&journal)?;
    ensure!(
        mcp.iter()
            .filter(|request| request["method"] == "skills/get")
            .count()
            == 1
    );
    ensure!(
        mcp.iter()
            .all(|request| request["method"] != "resources/read")
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("HTTP calls")?
            .iter()
            .all(|request| request.url.path() != "/chat/completions")
    );
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn managed_cli_reuses_its_persisted_mcp_root_after_the_resource_peer_disconnects()
-> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    let source_config = fixture.home.path().join("mcp-fixture.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&source_config)?)?;
    config["closeAfter"] = "resources/read".into();
    std::fs::write(&source_config, serde_json::to_vec(&config)?)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            let response = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                json!({"id":"cached-root","object":"chat.completion","model":"model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":null,
                        "tool_calls":[{"id":"read-workspace","type":"function","function":{"name":"read","arguments":"{\"path\":\".\"}"}}]},"finish_reason":"tool_calls"}],
                    "usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}})
            } else { text_result("completed with original MCP cache") };
            ResponseTemplate::new(200).set_body_json(response)
        }).with_priority(1).mount(&upstream).await;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let task = fixture
        .coding_command("cached-mcp-input")
        .args(["--skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let report = pending(&fixture, "cached-mcp-input").await?;
    let original = report
        .skill_consents
        .last()
        .context("original consent")?
        .source
        .clone();
    let approved = answer(
        &fixture,
        &report,
        &client.daemon_instance_id,
        "cached-mcp-input",
        "allow",
    )
    .await?;
    ensure!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    lines(&tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await??)?;
    let settled = wait_owner_finished(&client, "cached-mcp-input").await?;
    ensure!(
        settled.released
            && settled.run.as_ref().is_some_and(
                |run| run.status == bitrouter_orchestrator::core::session::RunStatus::Completed
            ),
        "{settled:?}"
    );
    ensure!(calls.load(Ordering::SeqCst) == 2);
    let http = upstream
        .received_requests()
        .await
        .context("model HTTP requests")?;
    let models = http
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .map(|request| serde_json::from_slice::<Value>(&request.body))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(models.len() == 2);
    for model in &models {
        let messages = model["messages"]
            .as_array()
            .context("actual model messages")?;
        ensure!(messages.iter().any(|message| message["role"] == "user"
            && message.to_string().contains(MARKER)
            && message.to_string().contains(&original.entry_digest)));
        ensure!(
            messages
                .iter()
                .filter(|message| message["role"] == "system")
                .all(|message| !message.to_string().contains(MARKER))
        );
    }
    let reads = requests(&journal)?
        .into_iter()
        .filter(|request| request["method"] == "resources/read")
        .collect::<Vec<_>>();
    ensure!(reads.len() == 1 && reads[0]["params"]["uri"] == ROOT);
    let stored = fixture.stored().await?;
    let snapshot = stored.snapshot.context("original checkpoint")?;
    let turn = snapshot.root_turn().context("original Turn")?;
    let selection = snapshot
        .skill_selection(&turn.agent_turn_id)
        .context("persisted skill selection")?;
    let candidate = selection
        .candidates
        .iter()
        .find(|candidate| {
            matches!(&candidate.source,
        bitrouter_orchestrator::core::skills::SkillSource::Mcp(held) if held == &original)
        })
        .context("original MCP binding")?;
    ensure!(
        candidate
            .content
            .as_ref()
            .is_some_and(|content| content.contains(MARKER))
    );
    ensure!(candidate.failure.is_none() && selection.failure.is_none());
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn managed_cli_skill_consent_binds_original_input_and_replays_only_committed_answers()
-> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    ensure!(client.skill_consent);
    let task = fixture
        .coding_command("skill-input")
        .args(["--skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let report = pending(&fixture, "skill-input").await?;
    let consent = report.skill_consents.last().context("consent")?;
    ensure!(consent.source.entry.extra.get("extra") == Some(&json!({"preserve":true})));
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("HTTP requests")?
            .iter()
            .all(|request| request.url.path() != "/chat/completions")
    );
    for (boot, key) in [
        ("wrong-boot", "skill-input"),
        (client.daemon_instance_id.as_str(), "wrong-input"),
    ] {
        let rejected = answer(&fixture, &report, boot, key, "allow").await?;
        ensure!(!rejected.status.success());
    }
    let mut invalid = consent.answer(true);
    invalid.entry_digest = "0".repeat(64);
    ensure!(
        client
            .request(Operation::AnswerSkill {
                name: "continued".into(),
                operation_id: "skill-input".into(),
                answer_operation_id: "wrong-entry".into(),
                answer: invalid,
            })
            .await
            .is_err()
    );
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    let accepted = answer(
        &fixture,
        &report,
        &client.daemon_instance_id,
        "skill-input",
        "allow",
    )
    .await?;
    ensure!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let answer_value: Value = serde_json::from_slice(&accepted.stdout)?;
    let answer_receipt = answer_value["payload"]["answer_receipt"].clone();
    ensure!(answer_receipt.is_object());
    let output = tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await??;
    lines(&output)?;
    let calls = upstream
        .received_requests()
        .await
        .context("HTTP requests")?;
    let models = calls
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .map(|request| serde_json::from_slice::<Value>(&request.body))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(models.len() == 1);
    let messages = models[0]["messages"].as_array().context("model messages")?;
    ensure!(messages.iter().any(|message| message["role"] == "user"
        && message.to_string().contains(MARKER)
        && message.to_string().contains(ROOT)
        && message.to_string().contains(SUPPORT)));
    ensure!(
        messages
            .iter()
            .filter(|message| message["role"] == "system")
            .all(|message| !message.to_string().contains(MARKER))
    );
    let reads: Vec<_> = requests(&journal)?
        .into_iter()
        .filter(|request| request["method"] == "resources/read")
        .collect();
    ensure!(reads.len() == 1 && reads[0]["params"]["uri"] == ROOT);
    let replay = answer(
        &fixture,
        &report,
        &client.daemon_instance_id,
        "skill-input",
        "allow",
    )
    .await?;
    ensure!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay_value: Value = serde_json::from_slice(&replay.stdout)?;
    ensure!(replay_value["payload"]["answer_receipt"] == answer_receipt);
    let conflict = answer(
        &fixture,
        &report,
        &client.daemon_instance_id,
        "skill-input",
        "deny",
    )
    .await?;
    ensure!(!conflict.status.success());
    let before = requests(&journal)?.len();
    fixture.stop().await?;
    fixture.start().await?;
    let restarted = Client::connect(&socket_path(&fixture.socket)).await?;
    let cold = answer(
        &fixture,
        &report,
        &restarted.daemon_instance_id,
        "skill-input",
        "allow",
    )
    .await?;
    ensure!(
        cold.status.success(),
        "{}",
        String::from_utf8_lossy(&cold.stderr)
    );
    let cold_value: Value = serde_json::from_slice(&cold.stdout)?;
    ensure!(cold_value["payload"]["answer_receipt"] == answer_receipt);
    ensure!(requests(&journal)?.len() == before);
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn managed_cli_denial_and_cancellation_never_read_unapproved_mcp_instructions() -> Result<()>
{
    for deny in [true, false] {
        let (mut fixture, upstream, journal) = fixture().await?;
        let client = Client::connect(&socket_path(&fixture.socket)).await?;
        let task = fixture
            .coding_command("unapproved")
            .args(["--skill", "mcp:skills-peer:skill://review/SKILL.md"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let report = pending(&fixture, "unapproved").await?;
        if deny {
            let denied = answer(
                &fixture,
                &report,
                &client.daemon_instance_id,
                "unapproved",
                "deny",
            )
            .await?;
            ensure!(
                denied.status.success(),
                "{}",
                String::from_utf8_lossy(&denied.stderr)
            );
        } else {
            let cancelled = fixture
                .command()
                .args([
                    "--json",
                    "task",
                    "managed-cancel",
                    "--session",
                    "continued",
                    "--request-id",
                    "unapproved",
                    "--server-instance",
                    &client.daemon_instance_id,
                    "--config",
                ])
                .arg(&fixture.config)
                .output()
                .await?;
            ensure!(
                cancelled.status.success(),
                "{}",
                String::from_utf8_lossy(&cancelled.stderr)
            );
        }
        let output =
            tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await??;
        ensure!(
            !output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let settled = fixture.inspect("unapproved").await?;
        ensure!(!settled.resident && settled.released);
        ensure!(settled.run.as_ref().is_some_and(|run| run.status
            == if deny {
                bitrouter_orchestrator::core::session::RunStatus::Failed
            } else {
                bitrouter_orchestrator::core::session::RunStatus::Cancelled
            }));
        ensure!(
            requests(&journal)?
                .iter()
                .all(|request| request["method"] != "resources/read")
        );
        let calls = upstream.received_requests().await.context("HTTP calls")?;
        let models: Vec<_> = calls
            .iter()
            .filter(|request| request.url.path() == "/chat/completions")
            .collect();
        ensure!(models.is_empty());
        ensure!(
            models
                .iter()
                .all(|request| !String::from_utf8_lossy(&request.body).contains(MARKER))
        );
        let late = answer(
            &fixture,
            &report,
            &client.daemon_instance_id,
            "unapproved",
            "allow",
        )
        .await?;
        ensure!(!late.status.success());
        ensure!(
            requests(&journal)?
                .iter()
                .all(|request| request["method"] != "resources/read")
        );
        fixture.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn a_corrupt_required_mcp_root_fails_before_the_model_request() -> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let task = fixture
        .coding_command("corrupt-root")
        .args(["--skill", "mcp:skills-peer:skill://review/SKILL.md"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let report = pending(&fixture, "corrupt-root").await?;
    let path = fixture.home.path().join("mcp-fixture.json");
    let mut peer: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    peer["files"][ROOT]["text"] = json!("CORRUPT_ROOT_MUST_NOT_ENTER_CONTEXT");
    std::fs::write(path, serde_json::to_vec(&peer)?)?;
    let approved = answer(
        &fixture,
        &report,
        &client.daemon_instance_id,
        "corrupt-root",
        "allow",
    )
    .await?;
    ensure!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    let output = tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await??;
    ensure!(!output.status.success());
    let settled = fixture.inspect("corrupt-root").await?;
    ensure!(settled.released && !settled.resident);
    let stored = fixture.stored().await?;
    let snapshot = stored.snapshot.context("original checkpoint")?;
    let candidate = snapshot
        .context_store
        .work
        .values()
        .filter_map(|work| work.skills.as_ref())
        .flat_map(|selection| &selection.candidates)
        .find(|candidate| {
            matches!(
                candidate.source,
                bitrouter_orchestrator::core::skills::SkillSource::Mcp(_)
            )
        })
        .context("MCP candidate")?;
    ensure!(candidate.selected && candidate.content.is_none());
    ensure!(selection_failure(&snapshot));
    ensure!(
        candidate
            .consents
            .last()
            .is_some_and(|consent| consent.approved == Some(true))
    );
    let calls = upstream.received_requests().await.context("HTTP calls")?;
    let models: Vec<_> = calls
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .collect();
    ensure!(models.is_empty());
    ensure!(models.iter().all(
        |request| !String::from_utf8_lossy(&request.body).contains(MARKER)
            && !String::from_utf8_lossy(&request.body).contains("CORRUPT_ROOT")
    ));
    let reads: Vec<_> = requests(&journal)?
        .into_iter()
        .filter(|request| request["method"] == "resources/read")
        .collect();
    ensure!(reads.len() == 1 && reads[0]["params"]["uri"] == ROOT);
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn task_run_waits_for_explicit_skill_consent_and_delivers_verified_user_data() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let (mut fixture, upstream, journal) = fixture().await?;
    let source_config = fixture.home.path().join("mcp-fixture.json");
    let mut source: Value = serde_json::from_slice(&std::fs::read(&source_config)?)?;
    source["pages"]["first"]["skills"] = json!([]);
    std::fs::write(&source_config, serde_json::to_vec(&source)?)?;
    let mut task = fixture
        .command()
        .args([
            "task",
            "run",
            "Review this workflow",
            "--skill",
            "mcp:skills-peer:skill://review/SKILL.md",
            "--read-only",
            "--max-output-tokens",
            "128",
            "--config",
        ])
        .arg(&fixture.config)
        .arg("--workspace")
        .arg(&fixture.workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = BufReader::new(task.stdout.take().context("task stdout")?);
    let accepted = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            ensure!(
                stdout.read_line(&mut line).await? > 0,
                "task exited without acceptance"
            );
            let value: Value = serde_json::from_str(&line)?;
            if value["type"] == "accepted" {
                return Ok::<_, anyhow::Error>(value);
            }
        }
    })
    .await??;
    let thread = accepted["thread_id"].as_str().context("thread identity")?;
    let turn = accepted["turn_id"].as_str().context("Turn identity")?;
    let boot = accepted["server_instance_id"]
        .as_str()
        .context("native boot")?;
    let waiting = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let output = fixture
                .command()
                .args([
                    "--json",
                    "threads",
                    "pending",
                    thread,
                    "--server-instance",
                    boot,
                    "--config",
                ])
                .arg(&fixture.config)
                .output()
                .await?;
            ensure!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout)?;
            if value["payload"]["pending"].is_object() {
                return Ok::<_, anyhow::Error>(value["payload"]["pending"].clone());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    ensure!(waiting["turn_id"] == turn && waiting["request"]["tool_name"] == "mcp_skill.activate");
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("HTTP calls")?
            .iter()
            .all(|request| request.url.path() != "/chat/completions")
    );
    let request = waiting["request"]["request_id"]
        .as_str()
        .context("consent ID")?;
    // A v18 task-run would auto-allow this exact input. Reject that wire
    // contract before it can answer a newly introduced consent request.
    let packet = bitrouter::agent_local::ThreadCommand {
        version: 18,
        command_id: "legacy-auto-allow".into(),
        server_instance_id: Some(boot.into()),
        operation: bitrouter::agent_local::Operation::Input {
            thread_id: thread.into(),
            turn_id: turn.into(),
            request_id: request.into(),
            approved: true,
            idempotency_key: "legacy-auto-allow".into(),
        },
    };
    let mut legacy = BufReader::new(
        tokio::net::UnixStream::connect(bitrouter::agent_local::socket_path(&fixture.socket))
            .await?,
    );
    use tokio::io::AsyncWriteExt;
    legacy
        .get_mut()
        .write_all(format!("{}\n", serde_json::to_string(&packet)?).as_bytes())
        .await?;
    let mut rejected = String::new();
    tokio::time::timeout(Duration::from_secs(5), legacy.read_line(&mut rejected)).await??;
    let rejected: Value = serde_json::from_str(&rejected)?;
    ensure!(rejected["type"] == "unsupported_version" && rejected["supported"] == 19);
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    let answer = fixture
        .command()
        .args([
            "--json",
            "threads",
            "answer",
            thread,
            "--turn",
            turn,
            "--request",
            request,
            "allow",
            "--request-id",
            "explicit-task-skill",
            "--server-instance",
            boot,
            "--config",
        ])
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(
        answer.status.success(),
        "{}",
        String::from_utf8_lossy(&answer.stderr)
    );
    let mut remaining = String::new();
    tokio::time::timeout(
        Duration::from_secs(20),
        stdout.read_to_string(&mut remaining),
    )
    .await??;
    let output = tokio::time::timeout(Duration::from_secs(20), task.wait_with_output()).await??;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        remaining
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|value| value["type"] == "terminal" && value["status"] == "completed")
    );
    let calls = upstream.received_requests().await.context("HTTP calls")?;
    let models = calls
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .map(|request| serde_json::from_slice::<Value>(&request.body))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(models.len() == 1);
    let messages = models[0]["messages"].as_array().context("model messages")?;
    ensure!(messages.iter().any(|message| message["role"] == "user"
        && message.to_string().contains(MARKER)
        && message.to_string().contains(ROOT)
        && message.to_string().contains(SUPPORT)));
    ensure!(
        messages
            .iter()
            .filter(|message| message["role"] == "system")
            .all(|message| !message.to_string().contains(MARKER))
    );
    ensure!(!models[0]["tools"].to_string().contains("\"shell\""));
    let reads: Vec<_> = requests(&journal)?
        .into_iter()
        .filter(|request| request["method"] == "resources/read")
        .collect();
    ensure!(reads.len() == 1 && reads[0]["params"]["uri"] == ROOT);
    fixture.stop().await?;
    std::fs::remove_file(fixture.home.path().join("mcp-fixture.json"))?;
    std::fs::remove_dir_all(&fixture.workspace)?;
    let count = requests(&journal)?.len();
    let output = explain(&fixture, &["--thread", thread, "--turn", turn], false).await?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let explained: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        explained["selector"]["kind"] == "native_thread"
            && explained["selector"]["thread_id"] == thread
            && explained["selector"]["turn_id"] == turn
    );
    let candidate = explained_candidate(&explained)?;
    ensure!(
        candidate["instructions"].is_object()
            && candidate["prepared_model_requests"]
                .as_array()
                .is_some_and(|requests| requests.len() == 1)
    );
    ensure!(candidate["source"]["binding"]["entry"]["uri"] == ROOT);
    ensure!(
        candidate["prepared_model_requests"][0]["attempts"][0]["model_result_received"] == true
    );
    ensure!(
        requests(&journal)?.len() == count
            && bitrouter::daemon::probe_status(&fixture.socket)
                .await?
                .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn native_http_command_approval_cannot_silently_approve_mcp_skill_content() -> Result<()> {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = reservation.local_addr()?;
    drop(reservation);
    let (mut fixture, upstream, journal) = fixture_at(Some(address)).await?;
    let api = format!("http://{address}/agent/v2");
    let client = reqwest::Client::new();
    let headers = {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("Bearer fixture-native-api-token-minimum24"),
        );
        let capabilities: Value = client
            .get(format!("{api}/capabilities"))
            .headers(headers.clone())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(capabilities["explicit_skill_consent"] == true);
        headers.insert(
            "x-bro-server-instance",
            reqwest::header::HeaderValue::from_str(
                capabilities["runtime"]["server_instance_id"]
                    .as_str()
                    .context("HTTP server identity")?,
            )?,
        );
        headers
    };
    let created: Value = client.post(format!("{api}/threads")).headers(headers.clone()).header("idempotency-key", "http-create")
        .json(&json!({"workspace":fixture.workspace,"model":"coding","read_only":true,"max_output_tokens":128,"skills":{"include":[{"kind":"mcp","server":"skills-peer","uri":"skill://review/SKILL.md"}],"exclude":[]}})).send().await?.error_for_status()?.json().await?;
    let thread = created["thread"]["thread_id"]
        .as_str()
        .context("HTTP Thread")?;
    let accepted: Value = client
        .post(format!("{api}/threads/{thread}/turns"))
        .headers(headers.clone())
        .header("idempotency-key", "http-input")
        .json(&json!({"prompt":"Review this workflow","mode":"start"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let turn = accepted["receipt"]["turn_id"]
        .as_str()
        .context("HTTP Turn")?;
    let turn_url = format!("{api}/threads/{thread}/turns/{turn}");
    let waiting: Value = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot: Value = client
                .get(&turn_url)
                .headers(headers.clone())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            ensure!(
                snapshot["turn"]["status"] != "failed"
                    && snapshot["turn"]["status"] != "recovery_required",
                "{snapshot}"
            );
            if snapshot["turn"]["pending_input"].is_object() {
                return Ok::<_, anyhow::Error>(snapshot["turn"].clone());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    ensure!(waiting["pending_input"]["tool_name"] == "mcp_skill.activate");
    let mut body =
        json!({"turn_id":turn,"request_id":waiting["pending_input"]["request_id"],"approved":true});
    let input_url = format!("{api}/threads/{thread}/inputs");
    let rejected = client
        .post(&input_url)
        .headers(headers.clone())
        .header("idempotency-key", "http-answer")
        .json(&body)
        .send()
        .await?;
    ensure!(rejected.status() == reqwest::StatusCode::BAD_REQUEST);
    ensure!(rejected.text().await?.contains("skill_consent=true"));
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("HTTP model calls")?
            .iter()
            .all(|request| request.url.path() != "/chat/completions")
    );
    body["skill_consent"] = json!(true);
    let receipt: Value = client
        .post(&input_url)
        .headers(headers.clone())
        .header("idempotency-key", "http-answer")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot: Value = client
                .get(&turn_url)
                .headers(headers.clone())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if snapshot["turn"]["status"] == "completed" {
                return Ok::<_, anyhow::Error>(());
            }
            ensure!(snapshot["turn"]["status"] != "failed", "{snapshot}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    let replay: Value = client
        .post(&input_url)
        .headers(headers)
        .header("idempotency-key", "http-answer")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for field in ["thread_id", "turn_id", "user_item_id", "queue_order"] {
        ensure!(receipt["receipt"][field] == replay["receipt"][field]);
    }
    ensure!(replay["receipt"]["status"] == "completed");
    let reads: Vec<_> = requests(&journal)?
        .into_iter()
        .filter(|request| request["method"] == "resources/read")
        .collect();
    ensure!(reads.len() == 1 && reads[0]["params"]["uri"] == ROOT);
    let calls = upstream
        .received_requests()
        .await
        .context("model requests")?;
    ensure!(
        calls
            .iter()
            .filter(|request| request.url.path() == "/chat/completions")
            .count()
            == 1
    );
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn cli_requires_an_unlisted_rootless_mcp_uri_and_delivers_its_exact_held_entry() -> Result<()>
{
    let (mut fixture, upstream, journal) = fixture().await?;
    let path = fixture.home.path().join("mcp-fixture.json");
    let mut peer: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let uri = "urn:review/SKILL.md?revision=1#instructions";
    let support = "urn:review/references/check.md?revision=2";
    let mut entry = peer["pages"]["first"]["skills"][0].clone();
    entry["uri"] = uri.into();
    entry["resources"][0]["uri"] = uri.into();
    entry["resources"][1]["uri"] = support.into();
    peer["pages"]["first"]["skills"] = json!([]);
    peer["get"] =
        json!({uri:{"resultType":"complete","skill":entry,"ttlMs":0,"cacheScope":"private"}});
    peer["files"] = json!({uri:peer["files"][ROOT].clone(),support:peer["files"][SUPPORT].clone()});
    std::fs::write(&path, serde_json::to_vec(&peer)?)?;
    let task = fixture
        .coding_command("rootless-uri-input")
        .args(["--read-only", "--skill", &format!("mcp:skills-peer:{uri}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let waiting = pending(&fixture, "rootless-uri-input").await?;
    ensure!(waiting.skill_consents.len() == 1);
    ensure!(waiting.skill_consents[0].source.entry.uri == uri);
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let approved = answer(
        &fixture,
        &waiting,
        &client.daemon_instance_id,
        "rootless-uri-input",
        "allow",
    )
    .await?;
    ensure!(approved.status.success());
    let output = tokio::time::timeout(Duration::from_secs(30), task.wait_with_output()).await??;
    lines(&output)?;
    let calls = upstream.received_requests().await.context("HTTP calls")?;
    let models: Vec<_> = calls
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .collect();
    ensure!(models.len() == 1);
    let request: Value = serde_json::from_slice(&models[0].body)?;
    let messages = request["messages"].as_array().context("model messages")?;
    ensure!(messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"].to_string().contains(uri)
            && message["content"].to_string().contains(MARKER)
    }));
    ensure!(
        messages
            .iter()
            .filter(|message| message["role"] == "system")
            .all(|message| !message["content"].to_string().contains(MARKER))
    );
    let reads: Vec<_> = requests(&journal)?
        .into_iter()
        .filter(|request| request["method"] == "resources/read")
        .collect();
    ensure!(reads.len() == 1 && reads[0]["params"]["uri"] == uri);
    fixture.stop().await?;
    let history = explain(
        &fixture,
        &[
            "--managed-session",
            "continued",
            "--request-id",
            "rootless-uri-input",
        ],
        false,
    )
    .await?;
    ensure!(history.status.success());
    let history: Value = serde_json::from_slice(&history.stdout)?;
    ensure!(explained_candidate(&history)?["source"]["binding"]["entry"]["uri"] == uri);
    ensure!(
        requests(&journal)?
            .into_iter()
            .filter(|request| request["method"] == "resources/read")
            .count()
            == 1
    );
    Ok(())
}

#[path = "skills/integration.rs"]
mod integration;

fn selection_failure(snapshot: &bitrouter_orchestrator::core::session::SessionSnapshot) -> bool {
    snapshot
        .context_store
        .work
        .values()
        .filter_map(|work| work.skills.as_ref())
        .any(|selection| selection.failure.is_some())
}
