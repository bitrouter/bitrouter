//! Exercise the installed headless commands with only a loopback gateway and private fixtures.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[path = "support/gateway.rs"]
mod gateway;
use gateway::{Mock, chunk, stream};

fn bro(home: &Path, workspace: &Path, endpoint: &str, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bro"));
    cmd.current_dir(workspace)
        .env_clear()
        .env("BRO_HOME", home)
        .env("HOME", home)
        .env("PATH", home)
        .env("BITROUTER_ENDPOINT", endpoint)
        .env("BITROUTER_API_KEY", "scoped-test-key")
        .env("OPENAI_API_KEY", "fixture-provider-secret")
        .env("BRO_TEST_CREDENTIAL", "fixture-peer-secret");
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        cmd.env("SystemRoot", root);
    }
    Ok(cmd.args(args).output()?)
}

fn event(out: &Output) -> Result<Value> {
    let line = std::str::from_utf8(&out.stdout)?
        .lines()
        .last()
        .context("missing CLI event")?;
    Ok(serde_json::from_str(line)?)
}

#[test]
fn unknown_paid_execution_fences_overlapping_grants_across_processes() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let parent = fixture.path().join("Project");
    let work = parent.join("owned");
    let nested = work.join("nested");
    let sibling = parent.join("sibling");
    let home = fixture.path().join("bro");
    fs::create_dir_all(&nested)?;
    fs::create_dir(&sibling)?;
    let body = r#"{"error":{"code":"permission_denied","message":"fixture denial","type":"permission_error"}}"#;
    let mock = Mock::start(vec![format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nX-BitRouter-Execution-Id: exec-f\r\nX-BitRouter-Request-Id: req-f\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )])?;
    let run = |workspace: &Path, args: &[&str]| bro(&home, workspace, &mock.endpoint, args);
    let first = run(&work, &["task", "run", "hello"])?;
    assert!(!first.status.success());
    let terminal = event(&first)?;
    assert_eq!(terminal["status"], "recovery_required");
    let id = terminal["thread_id"].as_str().context("thread")?;
    let turn = terminal["turn_id"].as_str().context("turn")?;
    let history = event(&run(&work, &["thread", "history", id])?)?;
    let step = &history["turns"][0]["steps"][0];
    assert_eq!(step["request_id"], "req-f");
    assert_eq!(step["execution_id"], "exec-f");
    assert_eq!(step["state"]["record"], "model_request");
    let resumed = run(&work, &["thread", "resume", id, "--turn", turn])?;
    assert!(!resumed.status.success());
    assert_eq!(event(&resumed)?["status"], "recovery_required");
    assert_eq!(event(&run(&work, &["thread", "history", id])?)?, history);
    for path in [&parent, &work, &nested] {
        let refused = run(path, &["task", "run", "overlap"])?;
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("owned by active or unresolved"));
    }
    let alias = fixture.path().join("project/owned/nested");
    if alias.is_dir() {
        let refused = run(&alias, &["thread", "create"])?;
        assert!(String::from_utf8_lossy(&refused.stderr).contains("owned by active or unresolved"));
    }
    for path in [&home, &home.join("threads"), fixture.path()] {
        let refused = run(path, &["thread", "create"])?;
        assert!(String::from_utf8_lossy(&refused.stderr).contains("BRO data directory"));
    }
    assert!(run(&sibling, &["thread", "create"])?.status.success());
    // Old checkpoints stored a terminal denial for the very same response.
    let mut legacy = history;
    legacy["turns"][0]["steps"][0]["state"] = json!({"record":"gateway_denied"});
    legacy["turns"][0]["outcome"]["status"] = json!("failed");
    fs::write(
        home.join("threads").join(format!("{id}.json")),
        serde_json::to_vec(&legacy)?,
    )?;
    assert_eq!(
        event(&run(&work, &["thread", "show", id])?)?["status"],
        "recovery_required"
    );
    assert!(
        !run(&work, &["thread", "start", id, "retry"])?
            .status
            .success()
    );
    assert!(!run(&nested, &["thread", "create"])?.status.success());
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[test]
fn approved_shell_can_work_only_in_its_grant_without_bro_credentials() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("work");
    let home = fixture.path().join("bro");
    fs::create_dir(&work)?;
    fs::write(work.join("seed"), "workspace data")?;
    fs::write(fixture.path().join("peer.sqlite"), "private peer fixture")?;
    let script = "cat seed > result && env > seen-env || exit 10; if cat ../peer.sqlite > read-attempt 2>/dev/null; then exit 11; fi; if (printf changed > ../peer.sqlite) 2>/dev/null; then exit 12; fi; printf confined";
    let call = json!({"role":"assistant","tool_calls":[{"index":0,"id":"shell1","type":"function","function":{"name":"shell","arguments":json!({"command":script}).to_string()}}]});
    let mock = Mock::start(vec![
        stream(chunk(call, Some("tool_calls")) + "data: [DONE]\n\n"),
        stream(chunk(json!({"content":"Finished"}), Some("stop")) + "data: [DONE]\n\n"),
    ])?;
    let first = bro(
        &home,
        &work,
        &mock.endpoint,
        &["task", "run", "check the workspace"],
    )?;
    ensure!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first = event(&first)?;
    assert!(!work.join("result").exists());
    let id = first["thread_id"].as_str().context("thread")?;
    let available = first["status"] == "waiting_for_input";
    if std::env::var_os("BRO_TEST_REQUIRE_WORKSPACE_SANDBOX").is_some() {
        assert!(
            available,
            "provisioned sandbox did not offer the shell tool: {first}"
        );
    }
    #[cfg(target_os = "macos")]
    if Path::new("/usr/bin/sandbox-exec").is_file()
        && Path::new("/System/Library/Sandbox/Profiles/dyld-support.sb").is_file()
    {
        assert!(available, "expected macOS workspace sandbox");
    }
    if available {
        let turn = first["turn_id"].as_str().context("turn")?;
        let approval = first["pending_input"]["request_id"]
            .as_str()
            .context("approval")?;
        let answered = bro(
            &home,
            &work,
            &mock.endpoint,
            &[
                "thread",
                "answer",
                id,
                "--turn",
                turn,
                "--request",
                approval,
                "allow",
            ],
        )?;
        ensure!(
            answered.status.success(),
            "{}",
            String::from_utf8_lossy(&answered.stderr)
        );
        assert_eq!(event(&answered)?["status"], "completed");
        assert_eq!(fs::read_to_string(work.join("result"))?, "workspace data");
        assert_eq!(fs::read_to_string(work.join("read-attempt"))?, "");
        let env = fs::read_to_string(work.join("seen-env"))?;
        for key in [
            "BITROUTER_API_KEY=",
            "OPENAI_API_KEY=",
            "BRO_TEST_CREDENTIAL=",
            "BRO_HOME=",
        ] {
            assert!(!env.lines().any(|line| line.starts_with(key)));
        }
    } else {
        assert_eq!(first["status"], "completed");
    }
    assert_eq!(
        fs::read_to_string(fixture.path().join("peer.sqlite"))?,
        "private peer fixture"
    );
    let requests = mock.join()?;
    let declarations = requests[0]["tools"].as_array().context("declarations")?;
    assert_eq!(
        declarations
            .iter()
            .any(|t| t["function"]["name"] == "shell"),
        available
    );
    let result = requests[1]["messages"]
        .as_array()
        .context("history")?
        .last()
        .context("result")?["content"]
        .as_str()
        .context("content")?;
    if available {
        let value: Value = serde_json::from_str(result)?;
        assert_eq!(value["exit_status"], 0);
        assert_eq!(value["stdout"], "confined");
    } else {
        assert!(result.contains("workspace sandbox unavailable"));
    }
    Ok(())
}

fn tool_reply(name: &str, arguments: Value, id: &str) -> String {
    let call = json!({"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]});
    stream(chunk(call, Some("tool_calls")) + "data: [DONE]\n\n")
}

fn approve(home: &Path, work: &Path, endpoint: &str, pending: &Value) -> Result<Output> {
    bro(
        home,
        work,
        endpoint,
        &[
            "thread",
            "answer",
            pending["thread_id"].as_str().context("thread")?,
            "--turn",
            pending["turn_id"].as_str().context("turn")?,
            "--request",
            pending["pending_input"]["request_id"]
                .as_str()
                .context("approval")?,
            "allow",
        ],
    )
}

#[test]
fn write_and_edit_publish_nested_file_before_checkpoint_confirmation() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let home = fixture.path().join("bro");
    let work = fixture.path().join("work");
    fs::create_dir(&work)?;
    let relative = "nested/child/note";
    let file = work.join(relative);
    let mock = Mock::start(vec![
        tool_reply("write", json!({"path":relative,"content":"first"}), "w"),
        tool_reply(
            "edit",
            json!({"path":relative,"edits":[{"oldText":"first","newText":"second"}]}),
            "e",
        ),
        stream(chunk(json!({"content":"Finished"}), Some("stop")) + "data: [DONE]\n\n"),
    ])?;
    let first = event(&bro(
        &home,
        &work,
        &mock.endpoint,
        &["task", "run", "edit note"],
    )?)?;
    assert_eq!(first["pending_input"]["tool_name"], "write");
    assert!(!file.exists());
    let second = event(&approve(&home, &work, &mock.endpoint, &first)?)?;
    assert_eq!(second["pending_input"]["tool_name"], "edit");
    assert_eq!(fs::read_to_string(&file)?, "first");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&file)?.permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640))?;
    }
    let done = event(&approve(&home, &work, &mock.endpoint, &second)?)?;
    assert_eq!(done["status"], "completed");
    assert_eq!(fs::read_to_string(&file)?, "second");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&file)?.permissions().mode() & 0o777, 0o640);
    }
    let id = done["thread_id"].as_str().context("thread")?;
    let saved = event(&bro(
        &home,
        &work,
        &mock.endpoint,
        &["thread", "history", id],
    )?)?;
    for step in &saved["turns"][0]["steps"].as_array().context("steps")?[..2] {
        assert_eq!(
            step["state"]["response"]["calls"][0]["state"]["effect"],
            "completed"
        );
    }
    assert_eq!(mock.join()?.len(), 3);
    Ok(())
}

#[cfg(unix)]
#[test]
fn directory_sync_failure_blocks_replay_before_and_after_file_publication() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for already_created in [false, true] {
        let fixture = tempfile::tempdir()?;
        let home = fixture.path().join("bro");
        let work = fixture.path().join("work");
        let parent = work.join("create-only");
        fs::create_dir_all(&parent)?;
        if already_created {
            fs::create_dir(parent.join("new"))?;
        }
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o300))?;
        if fs::File::open(&parent).is_ok() {
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
            return Ok(()); // A privileged test process bypasses the fixture's directory permissions.
        }
        let mock = Mock::start(vec![tool_reply(
            "write",
            json!({"path":"create-only/new/note","content":"value"}),
            "w",
        )])?;
        let first = event(&bro(
            &home,
            &work,
            &mock.endpoint,
            &["task", "run", "create note"],
        )?)?;
        let answered = approve(&home, &work, &mock.endpoint, &first)?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        assert!(!answered.status.success());
        let blocked = event(&answered)?;
        assert_eq!(blocked["status"], "recovery_required");
        assert!(parent.join("new").is_dir());
        assert_eq!(parent.join("new/note").exists(), already_created);
        let id = blocked["thread_id"].as_str().context("thread")?;
        let turn = blocked["turn_id"].as_str().context("turn")?;
        let saved = event(&bro(
            &home,
            &work,
            &mock.endpoint,
            &["thread", "history", id],
        )?)?;
        assert_eq!(
            saved["turns"][0]["steps"][0]["state"]["response"]["calls"][0]["state"]["effect"],
            "unknown"
        );
        let retry = bro(
            &home,
            &work,
            &mock.endpoint,
            &["thread", "resume", id, "--turn", turn],
        )?;
        assert_eq!(event(&retry)?["status"], "recovery_required");
        assert_eq!(parent.join("new/note").exists(), already_created);
        assert_eq!(mock.join()?.len(), 1);
    }
    Ok(())
}

#[test]
fn approval_is_bound_to_directory_identity_across_processes() -> Result<()> {
    for missing_id in [false, true] {
        let fixture = tempfile::tempdir()?;
        let work = fixture.path().join("work");
        let home = fixture.path().join("bro");
        fs::create_dir(&work)?;
        let mock = Mock::start(vec![tool_reply(
            "write",
            json!({"path":"note","content":"approved"}),
            "w",
        )])?;
        let pending = event(&bro(
            &home,
            &work,
            &mock.endpoint,
            &["task", "run", "write note"],
        )?)?;
        let id = pending["thread_id"].as_str().context("thread ID")?;
        let checkpoint = home.join("threads").join(format!("{id}.json"));
        if missing_id {
            let mut old: Value = serde_json::from_slice(&fs::read(&checkpoint)?)?;
            old.as_object_mut()
                .context("checkpoint object")?
                .remove("workspace_id");
            fs::write(&checkpoint, serde_json::to_vec(&old)?)?;
        } else {
            fs::rename(&work, fixture.path().join("original"))?;
            fs::create_dir(&work)?;
        }
        let before = fs::read(&checkpoint)?;
        let denied = approve(&home, &work, &mock.endpoint, &pending)?;
        ensure!(
            !denied.status.success()
                && String::from_utf8_lossy(&denied.stderr).contains("workspace")
        );
        assert_eq!(fs::read(checkpoint)?, before);
        assert!(!work.join("note").exists());
        assert!(
            bro(&home, &work, &mock.endpoint, &["thread", "history", id])?
                .status
                .success()
        );
        assert_eq!(mock.join()?.len(), 1);
    }
    Ok(())
}

#[test]
fn operator_check_reports_durable_evidence_and_requires_fresh_approval() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("work");
    let home = fixture.path().join("bro");
    fs::create_dir(&work)?;
    let final_reply = stream(chunk(json!({"content":"Ready."}), Some("stop")) + "data: [DONE]\n\n");
    let mock = Mock::start(vec![final_reply.clone(), final_reply])?;
    let run = |args: &[&str]| bro(&home, &work, &mock.endpoint, args);
    let script = "printf x >> runs; printf checked; printf detail >&2";
    for args in [
        vec!["task", "run", "hello", "--check", script, "--read-only"],
        vec!["task", "run", "hello", "--check", " "],
    ] {
        assert!(!run(&args)?.status.success());
    }
    let initial = run(&["task", "run", "hello", "--check", script])?;
    let result = event(&initial)?;
    let thread = result["thread_id"].as_str().context("thread")?;
    let turn = result["turn_id"].as_str().context("turn")?;
    assert_eq!(result["final_answer"], "Ready.");
    let evidence = &result["verification_evidence"];
    assert_eq!(evidence["command"], script);
    let available = result["verification"] == "passed";
    if available {
        assert!(initial.status.success());
        assert_eq!(result["status"], "completed");
        assert_eq!(evidence["stdout"], "checked");
        assert_eq!(evidence["stderr"], "detail");
        assert_eq!(evidence["exit_status"], 0);
        assert_eq!(evidence["timed_out"], false);
        assert_eq!(evidence["stdout_truncated"], false);
        assert!(evidence["interpreter"]["sandbox"].is_string());
    } else {
        assert!(!initial.status.success());
        assert_eq!(result["status"], "failed");
        assert_eq!(result["verification"], "unavailable");
        assert!(
            evidence["error"]
                .as_str()
                .context("refusal")?
                .contains("sandbox unavailable")
        );
    }
    #[cfg(target_os = "macos")]
    assert!(available);
    #[cfg(windows)]
    assert!(!available);
    let resumed = event(&run(&["thread", "resume", thread, "--turn", turn])?)?;
    assert_eq!(resumed, result);
    let observed = event(&run(&["thread", "show", thread])?)?;
    assert_eq!(&observed["verification_evidence"], evidence);

    let next = event(&run(&["thread", "start", thread, "another turn"])?)?;
    assert_eq!(next["status"], "waiting_for_input");
    let turn = next["turn_id"].as_str().context("second turn")?;
    let pending = &next["pending_input"];
    assert_eq!(pending["tool_name"], "shell");
    assert_eq!(
        serde_json::from_str::<Value>(pending["arguments"].as_str().context("args")?)?,
        json!({"command":script})
    );
    let req = pending["request_id"].as_str().context("approval")?;
    assert!(
        !run(&[
            "thread",
            "answer",
            thread,
            "--turn",
            turn,
            "--request",
            "stale",
            "allow"
        ])?
        .status
        .success()
    );
    let denied = run(&[
        "thread",
        "answer",
        thread,
        "--turn",
        turn,
        "--request",
        req,
        "deny",
    ])?;
    assert!(!denied.status.success());
    let denied = event(&denied)?;
    assert_eq!(denied["verification"], "denied");
    assert_eq!(denied["status"], "failed");
    assert!(denied["verification_evidence"]["exit_status"].is_null());
    if available {
        assert_eq!(fs::read_to_string(work.join("runs"))?, "x");
    } else {
        assert!(!work.join("runs").exists());
    }
    let checkpoint = home.join("threads").join(format!("{thread}.json"));
    let mut downgraded: Value = serde_json::from_slice(&fs::read(&checkpoint)?)?;
    assert_eq!(downgraded["format"], "bro-local/3");
    downgraded["format"] = json!("bro-local/1");
    fs::write(&checkpoint, serde_json::to_vec(&downgraded)?)?;
    let refused = run(&["thread", "resume", thread, "--turn", turn])?;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("verification binding"));
    let requests = mock.join()?;
    assert_eq!(requests.len(), 2);
    assert!(!serde_json::to_string(&requests)?.contains(script));
    Ok(())
}

#[test]
fn operator_check_exit_failure_preserves_bounded_stderr_and_answer() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("work");
    let home = fixture.path().join("bro");
    fs::create_dir(&work)?;
    let reply = stream(chunk(json!({"content":"Finished."}), Some("stop")) + "data: [DONE]\n\n");
    let mock = Mock::start(vec![reply])?;
    let out = bro(
        &home,
        &work,
        &mock.endpoint,
        &[
            "task",
            "run",
            "verify",
            "--check",
            "printf out; i=0; while [ $i -lt 5000 ]; do printf 1234567890123456 >&2; i=$((i+1)); done; exit 7",
        ],
    )?;
    assert!(!out.status.success());
    let result = event(&out)?;
    let requests = mock.join()?;
    assert_eq!(requests.len(), 1);
    assert_eq!(result["unknown_effect"], false);
    assert_eq!(result["status"], "failed");
    assert_eq!(result["final_answer"], "Finished.");
    if requests[0]["tools"]
        .as_array()
        .context("tools")?
        .iter()
        .any(|t| t["function"]["name"] == "shell")
    {
        assert_eq!(result["verification"], "failed");
        let ev = &result["verification_evidence"];
        assert_eq!(ev["exit_status"], 7);
        assert_eq!(ev["stdout"], "out");
        assert_eq!(ev["stderr_truncated"], true);
        assert!(ev["stderr"].as_str().context("stderr")?.len() < 80000);
    } else {
        assert_eq!(result["verification"], "unavailable");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn timed_out_operator_check_retains_unknown_effect_and_fences_resume() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("work");
    let home = fixture.path().join("bro");
    fs::create_dir(&work)?;
    let reply = stream(chunk(json!({"content":"Ready."}), Some("stop")) + "data: [DONE]\n\n");
    let mock = Mock::start(vec![reply])?;
    let run = |args: &[&str]| bro(&home, &work, &mock.endpoint, args);
    let result = run(&[
        "task",
        "run",
        "verify",
        "--check",
        "printf x >> marker; printf started; sleep 90",
    ])?;
    assert!(!result.status.success());
    let terminal = event(&result)?;
    if terminal["verification"] == "unavailable" {
        assert!(
            terminal["verification_evidence"]["error"]
                .as_str()
                .context("unavailable")?
                .contains("sandbox unavailable")
        );
        assert!(!work.join("marker").exists());
    } else {
        assert_eq!(terminal["status"], "recovery_required");
        assert_eq!(terminal["unknown_effect"], true);
        assert_eq!(terminal["final_answer"], "Ready.");
        assert_eq!(terminal["verification"], "failed");
        assert_eq!(terminal["verification_evidence"]["timed_out"], true);
        assert_eq!(terminal["verification_evidence"]["stdout"], "started");
        let thread = terminal["thread_id"].as_str().context("thread")?;
        let turn = terminal["turn_id"].as_str().context("turn")?;
        assert_eq!(
            event(&run(&["thread", "resume", thread, "--turn", turn])?)?,
            terminal
        );
        assert!(
            !run(&["thread", "start", thread, "continue"])?
                .status
                .success()
        );
        assert!(!run(&["thread", "create"])?.status.success());
        assert_eq!(fs::read_to_string(work.join("marker"))?, "x");
    }
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[test]
fn incomplete_model_answer_does_not_launch_operator_check() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("work");
    fs::create_dir(&work)?;
    let mock = Mock::start(vec![stream(
        chunk(json!({"content":"Partial"}), Some("length")) + "data: [DONE]\n\n",
    )])?;
    let out = bro(
        &fixture.path().join("bro"),
        &work,
        &mock.endpoint,
        &["task", "run", "verify", "--check", "printf x > marker"],
    )?;
    assert!(!out.status.success());
    let result = event(&out)?;
    assert_eq!(result["status"], "failed");
    assert_eq!(result["verification"], "unavailable");
    assert!(result["verification_evidence"].is_null());
    assert_eq!(result["final_answer"], "Partial");
    assert!(!work.join("marker").exists());
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[cfg(target_os = "macos")]
#[path = "support/macos_shell_copy.rs"]
mod macos_shell_copy;
