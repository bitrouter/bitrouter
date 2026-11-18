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
