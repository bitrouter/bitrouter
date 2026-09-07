use super::*;

async fn tap(directory: &Path) -> Result<WireTap> {
    Ok(WireTap {
        file: tokio::fs::File::create(directory.join("events.jsonl")).await?,
        process_id: uuid::Uuid::new_v4().to_string(),
        namespace: "profile".into(),
        scope_valid: true,
        sequence: 0,
        version: None,
    })
}

#[tokio::test]
async fn native_bytes_are_preserved_and_only_metadata_is_stored() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let tap = Arc::new(Mutex::new(tap(directory.path()).await?));
    let wire = b"{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"claude_code_version\":\"2.1.257\",\"apiKeySource\":\"fixture-secret\"}\n{\"type\":\"auth_status\",\"apiKey\":\"fixture-secret\"}\nnot-json\n";
    let mut output = vec![];
    copy_protocol(&wire[..], &mut output, "server", Arc::clone(&tap)).await?;
    assert_eq!(output, wire);
    let saved = tokio::fs::read_to_string(directory.path().join("events.jsonl")).await?;
    assert!(!saved.contains("fixture-secret"));
    let events = saved
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["payload"]["session_id"], "s");
    assert_eq!(events[1]["reason"], "native_cli_invalid_frame");
    assert_eq!(events[0]["process_id"], events[1]["process_id"]);
    Ok(())
}

#[tokio::test]
async fn partial_and_oversized_frames_forward_with_bounded_gap_evidence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let tap = Arc::new(Mutex::new(tap(directory.path()).await?));
    let mut wire = vec![b' '; MAX_RECORD_BYTES + 13];
    wire.extend_from_slice(b"\n{\"type\":\"result\",\"session_id\":\"s\"}\npartial");
    let mut output = vec![];
    copy_protocol(wire.as_slice(), &mut output, "server", tap).await?;
    assert_eq!(wire, output);
    let saved = tokio::fs::read_to_string(directory.path().join("events.jsonl")).await?;
    assert_eq!(saved.lines().count(), 3);
    assert!(saved.len() < 4096);
    Ok(())
}

#[test]
fn native_and_script_overrides_keep_their_execution_semantics() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = NativeRoot {
        harness: Harness::ClaudeCode,
        namespace: "profile".into(),
        directory: "/profile/projects".into(),
    };
    for original in ["/native/claude", "/runtime/cli.js", "/runtime/cli.ts"] {
        let mut env = HashMap::from([("CLAUDE_CODE_EXECUTABLE".into(), original.into())]);
        let capture = prepare_env(&mut env, directory.path(), Path::new("/bitrouter"), &root)?;
        if original == "/native/claude" && cfg!(unix) {
            assert!(capture);
            assert_eq!(env[UPSTREAM_ENV], original);
            assert_eq!(
                Path::new(&env["CLAUDE_CODE_EXECUTABLE"]),
                directory.path().join(PROXY_NAME)
            );
        } else {
            assert!(!capture);
            assert_eq!(
                env,
                HashMap::from([("CLAUDE_CODE_EXECUTABLE".into(), original.into())])
            );
        }
    }
    let original =
        json!({"_meta":{"claudeCode":{"options":{"settings":"absent.json","env":{"KEY":"kept"}}}}});
    let mut params = original.clone();
    instrument_scope(&mut params, Path::new("/new-spool"), "new-profile")?;
    assert_eq!(
        params.pointer("/_meta/claudeCode/options/settings"),
        original.pointer("/_meta/claudeCode/options/settings")
    );
    assert_eq!(
        params.pointer("/_meta/claudeCode/options/env/KEY"),
        Some(&json!("kept"))
    );
    assert_eq!(
        params.pointer(&format!("/_meta/claudeCode/options/env/{NAMESPACE_ENV}")),
        Some(&json!("new-profile"))
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn real_child_transport_keeps_arguments_exit_code_and_distinct_processes() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir()?;
    let executable = directory.path().join("claude");
    std::fs::write(
        &executable,
        "#!/bin/sh\n[ \"$1\" = 'literal ; argument' ] || exit 91\nread -r input\nprintf '%s\\n' '{\"type\":\"system\",\"subtype\":\"session_state_changed\",\"state\":\"idle\",\"session_id\":\"s\"}'\nexit 7\n",
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let spool = directory.path().join("spool");
    std::fs::create_dir(&spool)?;
    for _ in 0..2 {
        let (mut writer, input) = tokio::io::duplex(1024);
        writer
            .write_all(b"{\"type\":\"user\",\"uuid\":\"command\",\"message\":\"fixture-secret\"}\n")
            .await?;
        let mut output = vec![];
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_with(
                executable.clone(),
                vec!["literal ; argument".into()],
                spool.clone(),
                "profile".into(),
                true,
                input,
                &mut output,
            ),
        )
        .await??;
        assert_eq!(code, 7);
        assert_eq!(serde_json::from_slice::<Value>(&output)?["state"], "idle");
    }
    let paths = std::fs::read_dir(&spool)?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(paths.len(), 2);
    let mut ids = std::collections::BTreeSet::new();
    for path in paths {
        let saved = std::fs::read_to_string(path.path())?;
        assert!(!saved.contains("fixture-secret"));
        let events = saved
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        ids.insert(
            events[0]["process_id"]
                .as_str()
                .context("process id")?
                .to_owned(),
        );
        assert_eq!(events.last().context("exit")?["exit_code"], 7);
        assert_eq!(events.last().context("exit")?["clean"], false);
    }
    assert_eq!(ids.len(), 2);
    Ok(())
}

#[tokio::test]
async fn bundled_runtime_resolves_inside_the_selected_sdk() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let modules = directory.path().join("node_modules");
    let bin = modules.join(".bin");
    let adapter = modules.join("@agentclientprotocol/claude-agent-acp");
    let sdk = adapter.join("node_modules/@anthropic-ai/claude-agent-sdk");
    std::fs::create_dir_all(&bin)?;
    std::fs::create_dir_all(&sdk)?;
    std::fs::write(
        adapter.join("package.json"),
        r#"{"name":"@agentclientprotocol/claude-agent-acp","bin":{"claude-agent-acp":"entry.js"}}"#,
    )?;
    std::fs::write(adapter.join("entry.js"), "// adapter")?;
    std::fs::write(bin.join("claude-agent-acp.cmd"), "fixture shim")?;
    std::fs::write(
        sdk.join("package.json"),
        r#"{"name":"@anthropic-ai/claude-agent-sdk","main":"sdk.js"}"#,
    )?;
    std::fs::write(
        sdk.join("sdk.js"),
        "throw new Error('resolution must not execute SDK');",
    )?;
    let node = super::super::native_runtime::node(std::env::var_os("PATH"))?;
    let platform = Command::new(node)
        .arg("-p")
        .arg("JSON.stringify([process.platform,process.arch])")
        .output()
        .await?;
    let platform: Vec<String> = serde_json::from_slice(&platform.stdout)?;
    let name = format!(
        "@anthropic-ai/claude-agent-sdk-{}-{}",
        platform[0], platform[1]
    );
    let native = sdk.join("node_modules").join(&name);
    std::fs::create_dir_all(&native)?;
    let native = native.join(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    });
    std::fs::write(&native, "nested native fixture")?;
    let hoisted = modules.join(&name);
    std::fs::create_dir_all(&hoisted)?;
    std::fs::write(
        hoisted.join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        }),
        "wrong runtime",
    )?;
    let ambient = std::env::var_os("PATH").context("PATH")?;
    let search = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&ambient)))?;
    assert_eq!(
        resolve_upstream(None, Some(search), None).await?,
        std::fs::canonicalize(native)?
    );
    Ok(())
}
