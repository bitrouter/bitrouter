//! G11: public setup and lifecycle commands share the original native owner.
use super::*;
use tokio::io::{AsyncBufReadExt, BufReader};

fn path(value: &std::path::Path) -> Result<&str> {
    value.to_str().context("UTF-8 fixture path")
}

async fn cli(fixture: &Fixture, args: &[&str]) -> Result<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(25),
        fixture.command().args(args).output(),
    )
    .await??;
    ensure!(
        output.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn wait(
    fixture: &Fixture,
    args: &[&str],
    condition: impl Fn(&Value) -> bool,
) -> Result<Value> {
    let mut last = Value::Null;
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let value = cli(fixture, args).await?;
            if condition(&value) {
                return Ok::<_, anyhow::Error>(value);
            }
            last = value;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    result.map_err(|error| anyhow::anyhow!("native integration {args:?}: {error}; last={last}"))?
}

async fn load_after_restart(fixture: &Fixture, thread: &str, turn: &str) -> Result<Value> {
    let passive = fixture
        .command()
        .args(["threads", "show", thread, "--turn", turn])
        .output()
        .await?;
    ensure!(
        !passive.status.success()
            && String::from_utf8_lossy(&passive.stdout)
                .contains("permission profile is no longer granted"),
        "a query unexpectedly reconstructed a local workspace grant"
    );
    let loaded = cli(
        fixture,
        &[
            "threads",
            "load",
            thread,
            "--workspace",
            path(&fixture.workspace)?,
        ],
    )
    .await?;
    ensure!(loaded["payload"]["view"]["thread"]["thread_id"] == thread);
    Ok(loaded)
}

fn response(request: &Value, mut message: Value, finish: &str) -> ResponseTemplate {
    if request["stream"] == true {
        if let Some(calls) = message["tool_calls"].as_array_mut() {
            for (index, call) in calls.iter_mut().enumerate() {
                call["index"] = json!(index);
            }
        }
        let delta = json!({"id":"g11","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":message,"finish_reason":null}]});
        let stop = json!({"id":"g11","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":13,"completion_tokens":3,"total_tokens":16}});
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/event-stream")
            .set_body_string(format!("data: {delta}\n\ndata: {stop}\n\ndata: [DONE]\n\n"))
    } else {
        ResponseTemplate::new(200).set_body_json(json!({"id":"g11","object":"chat.completion","model":"model","choices":[{"index":0,"message":message,"finish_reason":finish}],"usage":{"prompt_tokens":13,"completion_tokens":3,"total_tokens":16}}))
    }
}

async fn begin(fixture: &Fixture, prompt: &str) -> Result<(String, String)> {
    let created = cli(
        fixture,
        &[
            "threads",
            "create",
            "--workspace",
            path(&fixture.workspace)?,
            "--model",
            "coding",
            "--request-id",
            "g11-create",
        ],
    )
    .await?;
    let thread = created["payload"]["snapshot"]["thread_id"]
        .as_str()
        .context("Thread")?
        .to_string();
    let input = fixture.home.path().join("g11-input.txt");
    std::fs::write(&input, prompt)?;
    let accepted = cli(
        fixture,
        &[
            "threads",
            "start",
            &thread,
            "--prompt-file",
            path(&input)?,
            "--request-id",
            "g11-start",
        ],
    )
    .await?;
    let turn = accepted["payload"]["receipt"]["turn_id"]
        .as_str()
        .context("Turn")?
        .to_string();
    Ok((thread, turn))
}

async fn consent(fixture: &Fixture, thread: &str, turn: &str) -> Result<Value> {
    let pending = wait(fixture, &["threads", "pending", thread], |value| {
        value["payload"]["pending"]["request"].is_object()
    })
    .await?;
    ensure!(pending["payload"]["pending"]["turn_id"] == turn);
    Ok(pending["payload"]["pending"].clone())
}

async fn explain_thread(fixture: &Fixture, thread: &str, turn: &str) -> Result<Value> {
    cli(
        fixture,
        &[
            "skills",
            "explain",
            "--thread",
            thread,
            "--turn",
            turn,
            "--config",
            path(&fixture.config)?,
        ],
    )
    .await
}

async fn models(upstream: &MockServer) -> Result<Vec<Value>> {
    upstream
        .received_requests()
        .await
        .context("HTTP calls")?
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .map(|request| serde_json::from_slice(&request.body).map_err(Into::into))
        .collect()
}

#[tokio::test]
async fn local_onboarding_to_routed_native_approval_uninstall_explain_and_cold_reconnect()
-> Result<()> {
    let (mut fixture, upstream, _journal) = fixture().await?;
    fixture.stop().await?;
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&fixture.config)?)?;
    settings
        .as_object_mut()
        .context("settings")?
        .remove("mcp_servers");
    settings
        .as_object_mut()
        .context("settings")?
        .remove("skills");
    std::fs::write(&fixture.config, serde_saphyr::to_string(&settings)?)?;
    let source = fixture.home.path().join("review");
    std::fs::create_dir_all(source.join("references"))?;
    let body = "---\nname: review\ndescription: local workflow\nallowed-tools: shell\n---\nG11_LOCAL_ORIGINAL\n";
    std::fs::write(source.join("SKILL.md"), body)?;
    std::fs::write(source.join("references/check.md"), "original reference")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("POST")).and(path_matcher("/chat/completions")).respond_with(move |request:&wiremock::Request|{
        let Ok(value)=serde_json::from_slice::<Value>(&request.body) else{return ResponseTemplate::new(400);};
        if counter.fetch_add(1,Ordering::SeqCst)==0 {
            response(&value,json!({"role":"assistant","content":null,"tool_calls":[{"id":"g11-write","type":"function","function":{"name":"write","arguments":"{\"path\":\"approved.txt\",\"content\":\"one approved effect\"}"}}]}),"tool_calls")
        } else {response(&value,json!({"role":"assistant","content":"local integration completed"}),"stop")}
    }).with_priority(1).mount(&upstream).await;
    let setup = cli(
        &fixture,
        &[
            "init",
            "--yes",
            "--config",
            path(&fixture.config)?,
            "--install-skill",
            path(&source)?,
            "--workspace",
            path(&fixture.workspace)?,
            "--after",
            "serve",
        ],
    )
    .await?;
    ensure!(setup["daemon"]["status"] == "ready");
    ensure!(
        setup["resources"]
            .as_array()
            .context("resources")?
            .iter()
            .any(|row| row["kind"] == "skill" && row["status"] == "installed")
    );
    let reused = cli(&fixture, &[]).await?;
    ensure!(reused["pid"] == setup["daemon"]["pid"]);
    let (thread, turn) = begin(&fixture, "review changes and write the approved result").await?;
    let waiting = consent(&fixture, &thread, &turn).await?;
    ensure!(waiting["request"]["tool_name"] == "write");
    ensure!(!fixture.workspace.join("approved.txt").exists());
    let request = waiting["request"]["request_id"]
        .as_str()
        .context("request")?;
    let before = explain_thread(&fixture, &thread, &turn).await?;
    let original = explained_candidate(&before)?.clone();
    let mut observer = fixture
        .command()
        .args(["threads", "events", &thread, "--follow"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut output = BufReader::new(observer.stdout.take().context("observer")?);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line)).await??;
    ensure!(serde_json::from_str::<Value>(&line)?["kind"] == "native_event");
    observer.start_kill()?;
    observer.wait().await?;
    ensure!(consent(&fixture, &thread, &turn).await?["request"]["request_id"] == request);
    std::fs::write(
        source.join("SKILL.md"),
        body.replace("G11_LOCAL_ORIGINAL", "G11_LOCAL_UPDATED"),
    )?;
    cli(
        &fixture,
        &[
            "skills",
            "update",
            "review",
            "--workspace",
            path(&fixture.workspace)?,
        ],
    )
    .await?;
    cli(
        &fixture,
        &[
            "skills",
            "uninstall",
            "review",
            "--workspace",
            path(&fixture.workspace)?,
        ],
    )
    .await?;
    let stale = fixture
        .command()
        .args([
            "threads",
            "answer",
            &thread,
            "--turn",
            "wrong-turn",
            "--request",
            request,
            "allow",
        ])
        .output()
        .await?;
    ensure!(!stale.status.success() && !fixture.workspace.join("approved.txt").exists());
    cli(
        &fixture,
        &[
            "threads",
            "answer",
            &thread,
            "--turn",
            &turn,
            "--request",
            request,
            "allow",
            "--request-id",
            "g11-allow",
        ],
    )
    .await?;
    wait(
        &fixture,
        &["threads", "show", &thread, "--turn", &turn],
        |value| value["payload"]["snapshot"]["status"] == "completed",
    )
    .await?;
    ensure!(
        std::fs::read_to_string(fixture.workspace.join("approved.txt"))? == "one approved effect"
    );
    let sent = models(&upstream).await?;
    ensure!(
        sent.len() == 2
            && sent.iter().all(|request| request["model"] == "model"
                && request.to_string().contains("G11_LOCAL_ORIGINAL")
                && !request.to_string().contains("G11_LOCAL_UPDATED"))
    );
    let completed = explain_thread(&fixture, &thread, &turn).await?;
    ensure!(explained_candidate(&completed)?["source"] == original["source"]);
    fixture.stop().await?;
    let cold = explain_thread(&fixture, &thread, &turn).await?;
    ensure!(explained_candidate(&cold)? == explained_candidate(&completed)?);
    ensure!(models(&upstream).await?.len() == 2);
    fixture.start().await?;
    let loaded = load_after_restart(&fixture, &thread, &turn).await?;
    let recovery = &loaded["payload"]["view"]["recovery"];
    ensure!(
        recovery.is_object(),
        "explicit load lost recovery inspection: {loaded}"
    );
    let recovered = fixture.home.path().join("g11-recovery.json");
    std::fs::write(
        &recovered,
        serde_json::to_vec(&json!({
            "source_server_instance_id": recovery["source_server_instance_id"],
            "source_cursor": recovery["source_cursor"],
            "idempotency_key": "g11-recover-original"
        }))?,
    )?;
    cli(
        &fixture,
        &["threads", "recover", &thread, "--from", path(&recovered)?],
    )
    .await?;
    let reconnected = cli(&fixture, &["threads", "show", &thread, "--turn", &turn]).await?;
    ensure!(reconnected["payload"]["snapshot"]["status"] == "completed");
    ensure!(models(&upstream).await?.len() == 2);
    ensure!(
        explained_candidate(&explain_thread(&fixture, &thread, &turn).await?)?
            == explained_candidate(&completed)?
    );
    fixture.stop().await
}

#[tokio::test]
async fn mcp_onboarding_to_native_consent_lazy_files_and_revoked_capability_after_restart()
-> Result<()> {
    let (mut fixture, upstream, journal) = fixture().await?;
    fixture.stop().await?;
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&fixture.config)?)?;
    let entry = settings["mcp_servers"]["skills-peer"].clone();
    let policy = settings["skills"].clone();
    settings
        .as_object_mut()
        .context("settings")?
        .remove("mcp_servers");
    settings
        .as_object_mut()
        .context("settings")?
        .remove("skills");
    std::fs::write(&fixture.config, serde_saphyr::to_string(&settings)?)?;
    let input = fixture.home.path().join("setup-mcp.json");
    std::fs::write(&input, serde_json::to_vec(&entry)?)?;
    let setup = cli(
        &fixture,
        &[
            "init",
            "--yes",
            "--config",
            path(&fixture.config)?,
            "--mcp-name",
            "skills-peer",
            "--mcp-from",
            path(&input)?,
            "--after",
            "exit",
        ],
    )
    .await?;
    ensure!(setup["resources"][0]["status"] == "saved");
    settings = serde_saphyr::from_str(&std::fs::read_to_string(&fixture.config)?)?;
    ensure!(
        settings.get("skills").is_none(),
        "saving an MCP entry must not grant skill-source authority"
    );
    // Explicit operator input is separate from discovery/content consent.
    settings["skills"] = policy;
    std::fs::write(&fixture.config, serde_saphyr::to_string(&settings)?)?;
    std::fs::write(&journal, "")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("POST")).and(path_matcher("/chat/completions")).respond_with(move |request:&wiremock::Request|{
        let Ok(value)=serde_json::from_slice::<Value>(&request.body) else{return ResponseTemplate::new(400);};
        if counter.fetch_add(1,Ordering::SeqCst)==0 {
            let Some(id)=value["tools"].as_array().and_then(|tools|tools.iter().find(|tool|tool["function"]["name"]=="skill_read")).and_then(|tool|tool["function"]["parameters"]["properties"]["skill_id"]["enum"][0].as_str()) else{return ResponseTemplate::new(400);};
            response(&value,json!({"role":"assistant","content":null,"tool_calls":[{"id":"g11-support","type":"function","function":{"name":"skill_read","arguments":json!({"skill_id":id,"reference":"references/check.md"}).to_string()}}]}),"tool_calls")
        } else {response(&value,json!({"role":"assistant","content":"MCP integration completed"}),"stop")}
    }).with_priority(1).mount(&upstream).await;
    let started = cli(&fixture, &[]).await?;
    ensure!(started["status"] == "ready");
    let (thread, turn) = begin(&fixture, "review using the explicitly permitted source").await?;
    let waiting = consent(&fixture, &thread, &turn).await?;
    ensure!(waiting["request"]["tool_name"] == "mcp_skill.activate");
    ensure!(
        requests(&journal)?
            .iter()
            .all(|request| request["method"] != "resources/read")
    );
    ensure!(models(&upstream).await?.is_empty());
    let id = waiting["request"]["request_id"]
        .as_str()
        .context("consent ID")?;
    cli(
        &fixture,
        &[
            "threads",
            "answer",
            &thread,
            "--turn",
            &turn,
            "--request",
            id,
            "allow",
            "--request-id",
            "g11-content-consent",
        ],
    )
    .await?;
    wait(
        &fixture,
        &["threads", "show", &thread, "--turn", &turn],
        |value| value["payload"]["snapshot"]["status"] == "completed",
    )
    .await?;
    let sent = models(&upstream).await?;
    ensure!(sent.len() == 2 && sent.iter().all(|request| request["model"] == "model"));
    ensure!(
        sent[0].to_string().contains(MARKER)
            && !sent[0].to_string().contains("BEGIN VERIFIED FILE DATA")
    );
    let messages = sent[1]["messages"].as_array().context("model messages")?;
    ensure!(messages.iter().any(|message| message["role"] == "user"
        && message["content"].as_str().is_some_and(
            |text| text.contains("BEGIN VERIFIED FILE DATA") && text.ends_with("support\n")
        )));
    ensure!(
        messages
            .iter()
            .filter(|message| message["role"] == "system")
            .all(|message| !message.to_string().contains(MARKER)
                && !message.to_string().contains("BEGIN VERIFIED FILE DATA"))
    );
    let reads: Vec<_> = requests(&journal)?
        .into_iter()
        .filter(|request| request["method"] == "resources/read")
        .collect();
    ensure!(
        reads.len() == 2
            && reads[0]["params"]["uri"] == ROOT
            && reads[1]["params"]["uri"] == SUPPORT
    );
    let original = explain_thread(&fixture, &thread, &turn).await?;
    let candidate = explained_candidate(&original)?.clone();
    ensure!(
        candidate["source"]["kind"] == "mcp"
            && candidate["supporting_files"][0]["reference"] == SUPPORT
    );
    fixture.stop().await?;
    let original_requests = requests(&journal)?.len();
    let cold = explain_thread(&fixture, &thread, &turn).await?;
    ensure!(
        explained_candidate(&cold)? == &candidate
            && requests(&journal)?.len() == original_requests
            && models(&upstream).await?.len() == 2
    );
    let peer = fixture.home.path().join("mcp-fixture.json");
    let mut current: Value = serde_json::from_slice(&std::fs::read(&peer)?)?;
    current["capabilities"] = json!({"resources":{}});
    current["files"][SUPPORT]["text"] = json!("changed support");
    std::fs::write(&peer, serde_json::to_vec(&current)?)?;
    cli(&fixture, &[]).await?;
    load_after_restart(&fixture, &thread, &turn).await?;
    ensure!(
        cli(&fixture, &["threads", "show", &thread, "--turn", &turn]).await?["payload"]["snapshot"]
            ["status"]
            == "completed"
    );
    ensure!(explained_candidate(&explain_thread(&fixture, &thread, &turn).await?)? == &candidate);
    let before = requests(&journal)?.len();
    let rejected = tokio::time::timeout(
        Duration::from_secs(20),
        fixture
            .command()
            .args([
                "task",
                "run",
                "required after revoked capability",
                "--read-only",
                "--workspace",
                path(&fixture.workspace)?,
                "--model",
                "coding",
                "--skill",
                "mcp:skills-peer:skill://review/SKILL.md",
                "--config",
                path(&fixture.config)?,
            ])
            .output(),
    )
    .await??;
    ensure!(
        !rejected.status.success(),
        "revoked upstream capability was ignored"
    );
    ensure!(models(&upstream).await?.len() == 2);
    ensure!(
        requests(&journal)?[before..]
            .iter()
            .all(|request| !matches!(
                request["method"].as_str(),
                Some("skills/list" | "skills/get" | "resources/read")
            ))
    );
    fixture.stop().await
}

use wiremock::matchers::path as path_matcher;
