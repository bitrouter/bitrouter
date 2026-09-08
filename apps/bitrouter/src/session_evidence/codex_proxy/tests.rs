use super::*;

async fn tap(directory: &Path) -> Result<WireTap> {
    Ok(WireTap {
        file: File::create(directory.join("events.jsonl")).await?,
        requests: BTreeMap::new(),
        sequence: 0,
    })
}

#[tokio::test]
async fn wire_tap_keeps_native_correlation_and_excludes_configuration_secrets() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut tap = tap(directory.path()).await?;
    assert!(
        tap.select(
            "client",
            &json!({"id":0,"method":"account/login/start","params":{"apiKey":"private"}})
        )?
        .is_none()
    );
    let request = tap.select("client", &json!({"id":1,"method":"thread/start","params":{"cwd":"/work","config":{"api_key":"private"}}}))?.context("request")?;
    assert_eq!(request["payload"], json!({"cwd":"/work"}));
    let response = tap.select("server", &json!({"id":1,"result":{"thread":{"id":"thread-1","sessionId":"tree-1","parentThreadId":"parent"},"modelProvider":"configured"}}))?.context("response")?;
    assert_eq!(response["method"], "thread/start");
    assert_eq!(response["payload"]["thread"]["parentThreadId"], "parent");
    assert!(
        tap.select("server", &json!({"id":0,"result":{"apiKey":"private"}}))?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn steering_preserves_both_requested_and_accepted_native_turns() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut tap = tap(directory.path()).await?;
    let request = tap.select("client", &json!({"id":"steer-1","method":"turn/steer","params":{
        "threadId":"root","expectedTurnId":"turn-a","input":[],"config":{"api_key":"private"}
    }}))?.context("steer request")?;
    let response = tap
        .select(
            "server",
            &json!({"id":"steer-1","result":{
                "turnId":"turn-a","modelProvider":"private"
            }}),
        )?
        .context("steer response")?;
    assert_eq!(
        request["payload"],
        json!({"threadId":"root","expectedTurnId":"turn-a","input":[]})
    );
    assert_eq!(response["payload"], json!({"turnId":"turn-a"}));
    assert_eq!(response["operation_id"], request["operation_id"]);
    assert_eq!(response["method"], "turn/steer");
    Ok(())
}

#[tokio::test]
async fn lifecycle_tap_retains_revert_boundaries_and_marks_inline_resume_history() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut tap = tap(directory.path()).await?;
    let request = tap
        .select(
            "client",
            &json!({"id":1,"method":"thread/revert","params":{
                "threadId":"root", "beforeTurnId":"excluded", "config":{"api_key":"private"}
            }}),
        )?
        .context("revert")?;
    assert_eq!(
        request["payload"],
        json!({"threadId":"root","beforeTurnId":"excluded"})
    );
    let path = directory.path().join("rollout-root_reverted.jsonl");
    let response = tap
        .select(
            "server",
            &json!({"id":1,"result":{"thread":{"id":"root","path":path}}}),
        )?
        .context("response")?;
    assert_eq!(response["method"], "thread/revert");
    assert_eq!(response["payload"]["thread"]["path"], json!(path));
    for method in [
        "thread/reverted",
        "thread/archived",
        "thread/unarchived",
        "thread/deleted",
    ] {
        assert!(
            tap.select(
                "server",
                &json!({"method":method,"params":{"threadId":"root"}})
            )?
            .is_some()
        );
    }
    let resume = tap
        .select(
            "client",
            &json!({"id":2,"method":"thread/resume","params":{
                "threadId":"ignored", "path":path,"history":[{"type":"message","content":"opaque"}]
            }}),
        )?
        .context("resume")?;
    assert_eq!(resume["payload"]["bitrouter_inline_history"], true);
    assert!(resume["payload"].get("history").is_none());
    Ok(())
}

#[tokio::test]
async fn forwarding_preserves_bytes_and_commits_the_full_event_first() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let tap = Arc::new(Mutex::new(tap(directory.path()).await?));
    let wire = b"{ \"method\": \"item/completed\", \"params\": {\"threadId\":\"t\",\"item\":{\"id\":\"i\",\"type\":\"contextCompaction\"}} }\n";
    let mut output = Vec::new();
    copy_protocol(&wire[..], &mut output, "server", tap).await?;
    assert_eq!(output, wire);
    let saved = tokio::fs::read_to_string(directory.path().join("events.jsonl")).await?;
    let event: Value = serde_json::from_str(saved.trim())?;
    assert_eq!(event["payload"]["threadId"], "t");
    assert_eq!(event["payload"]["item"]["type"], "contextCompaction");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn process_proxy_uses_selected_runtime_and_handles_server_eof() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir()?;
    let executable = directory.path().join("codex");
    tokio::fs::write(&executable, b"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli fixture'; exit 0; fi\nread request\nprintf '%s\\n' '{\"method\":\"thread/started\",\"params\":{\"thread\":{\"id\":\"fixture\"}}}'\n").await?;
    tokio::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).await?;
    let mut output = Vec::new();
    let (mut writer, input) = tokio::io::duplex(1024);
    writer
        .write_all(b"{\"method\":\"thread/start\",\"id\":1,\"params\":{}}\n")
        .await?;
    // The manager intentionally leaves stdin open after the server exits.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_with(
            RuntimeCommand {
                program: executable.clone(),
                args: vec![],
            },
            directory.path().join("spool"),
            input,
            &mut output,
        ),
    )
    .await??;
    assert_eq!(
        serde_json::from_slice::<Value>(&output)?["params"]["thread"]["id"],
        "fixture"
    );
    let path = std::env::join_paths([directory.path()])?;
    assert_eq!(
        resolve_upstream(Some(executable.clone()), Some(path), None)
            .await?
            .program,
        executable
    );
    Ok(())
}

#[tokio::test]
async fn resolved_server_requests_retire_pending_correlations() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut tap = tap(directory.path()).await?;
    tap.select(
        "server",
        &json!({"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"t"}}),
    )?;
    assert_eq!(tap.requests.len(), 1);
    let event = tap
        .select(
            "server",
            &json!({"method":"serverRequest/resolved","params":{"threadId":"t","requestId":7}}),
        )?
        .context("resolution event")?;
    assert_eq!(event["payload"]["requestId"], 7);
    assert!(tap.requests.is_empty());
    assert!(
        tap.select("client", &json!({"id":7,"result":{"decision":"accept"}}))?
            .is_none()
    );
    assert!(
        tap.select(
            "server",
            &json!({"method":"error","params":{"threadId":"t","willRetry":false}})
        )?
        .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn bundled_npm_runtime_uses_node_with_a_literal_script_argument() -> Result<()> {
    let directory = tempfile::Builder::new()
        .prefix("adapter space # % ")
        .tempdir()?;
    let modules = directory.path().join("node_modules");
    let bin = modules.join(".bin");
    let package = modules.join("@agentclientprotocol/codex-acp");
    let script_dir = package.join("node_modules/@openai/codex/bin");
    for path in [&bin, &package, &script_dir] {
        std::fs::create_dir_all(path)?;
    }
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"@agentclientprotocol/codex-acp","bin":{"codex-acp":"entry.js"}}"#,
    )?;
    let entry = package.join("entry.js");
    std::fs::write(&entry, "// adapter fixture")?;
    std::fs::write(bin.join("codex-acp.cmd"), "fixture shim")?;
    let script = script_dir.join("codex.js");
    std::fs::write(&script, "// fixture")?;
    // A different hoisted dependency must not win over the adapter's own one.
    let hoisted = modules.join("@openai/codex/bin");
    std::fs::create_dir_all(&hoisted)?;
    std::fs::write(hoisted.join("codex.js"), "// wrong version")?;
    let native_path = std::env::var_os("PATH").context("PATH")?;
    let search = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&native_path)),
    )?;
    let runtime = resolve_upstream(None, Some(search.clone()), None).await?;
    assert_eq!(runtime.args.len(), 1);
    assert_eq!(
        std::fs::canonicalize(&runtime.args[0])?,
        std::fs::canonicalize(script)?
    );
    assert!(runtime.program.is_file());
    let explicit =
        resolve_upstream(Some(runtime.program.clone()), Some(search.clone()), None).await?;
    assert_eq!(explicit.program, runtime.program);
    assert!(explicit.args.is_empty());
    let configured = resolve_upstream(None, Some(search), Some(entry)).await?;
    assert_eq!(configured.args, runtime.args);
    Ok(())
}

#[cfg(unix)]
#[test]
fn global_adapter_symlink_identifies_its_real_package() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let package = directory
        .path()
        .join("lib/node_modules/@agentclientprotocol/codex-acp");
    let bin = directory.path().join("bin");
    std::fs::create_dir_all(&package)?;
    std::fs::create_dir_all(&bin)?;
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"@agentclientprotocol/codex-acp","bin":"entry.js"}"#,
    )?;
    let entry = package.join("entry.js");
    std::fs::write(&entry, "fixture")?;
    std::os::unix::fs::symlink(&entry, bin.join("codex-acp"))?;
    let search = std::env::join_paths([bin])?;
    assert_eq!(
        adapter_entry(None, Some(&search))?,
        std::fs::canonicalize(entry)?
    );
    Ok(())
}
