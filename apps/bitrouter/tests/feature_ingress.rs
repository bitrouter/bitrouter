//! Optional ingress capabilities must not create unavailable service resources.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::daemon::{self, DaemonCommand, DaemonResponse};
use bitrouter::paths::ConfigSource;

fn command(directory: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .current_dir(directory)
        .env_clear()
        .env("HOME", directory)
        .env("USERPROFILE", directory)
        .env("BITROUTER_HOME", directory.join("runtime"))
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn offline_skills_commands_preserve_valid_and_invalid_entries() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let created = tokio::time::timeout(
        Duration::from_secs(20),
        command(directory.path())
            .args([
                "skills",
                "init",
                "retained",
                "--output",
                "skills/retained/SKILL.md",
            ])
            .output(),
    )
    .await??;
    ensure!(
        created.status.success(),
        "skill initialization failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&created.stdout)?;
    ensure!(report["created"] == true);
    let skill = directory.path().join("skills/retained/SKILL.md");
    let original = std::fs::read(&skill)?;

    let broken = directory.path().join("skills/broken");
    std::fs::create_dir_all(&broken)?;
    std::fs::write(broken.join("SKILL.md"), "no frontmatter\n")?;
    let listed = tokio::time::timeout(
        Duration::from_secs(20),
        command(directory.path()).args(["skills", "list"]).output(),
    )
    .await??;
    ensure!(
        listed.status.success(),
        "skill inspection failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&listed.stdout)?;
    let rows = report["skills"]
        .as_array()
        .context("missing skills report")?;
    ensure!(rows.len() == 2);
    ensure!(rows[0]["name"] == "broken" && rows[0]["valid"] == false);
    ensure!(
        rows[0]["problem"] == "SKILL.md has no YAML frontmatter block",
        "invalid skill disappeared or changed its diagnostic"
    );
    ensure!(rows[1]["name"] == "retained" && rows[1]["valid"] == true);
    ensure!(rows[1].get("problem").is_none());
    ensure!(std::fs::read(&skill)? == original);
    Ok(())
}

#[tokio::test]
async fn unavailable_database_backend_rejects_before_service_activation() -> Result<()> {
    let mut cases = Vec::new();
    if !cfg!(feature = "db-postgres") {
        cases.extend([("postgres", "db-postgres"), ("postgresql", "db-postgres")]);
    }
    if !cfg!(feature = "db-mysql") {
        cases.push(("mysql", "db-mysql"));
    }
    for (scheme, feature) in cases {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bitrouter.yaml");
        let url = format!("{scheme}://fixture:private-password@127.0.0.1:1/fixture");
        let raw = format!(
            "inherit_defaults: false\nregistry:\n  enabled: false\ndatabase:\n  url: '{url}'\n"
        );
        std::fs::write(&path, &raw)?;
        let source = ConfigSource::File(path.clone());
        let config = bitrouter::config::parse_with(&raw, |_| None)?;
        // A reduced artifact still decodes the full saved configuration.
        ensure!(config.database.url == url);
        let connect = bitrouter::db::connect(&url)
            .await
            .err()
            .context("driver accepted")?;
        let assembly = bitrouter::build_app_with_path(&config, Some(&path))
            .await
            .err()
            .context("assembly accepted unavailable driver")?;
        let reload = bitrouter::reload::load_configuration_baseline(&source)
            .await
            .err()
            .context("reload accepted unavailable driver")?;
        let preflight = bitrouter::upgrade_preflight::Preflight::check_explicit(&source)
            .await
            .err()
            .context("preflight accepted unavailable driver")?;
        for diagnostic in [
            connect.to_string(),
            format!("{assembly:#}"),
            format!("{reload:#}"),
            format!("{preflight:#}"),
        ] {
            ensure!(diagnostic.contains(feature), "{diagnostic}");
            ensure!(
                !diagnostic.contains("private-password"),
                "credential in diagnostic"
            );
        }
        ensure!(std::fs::read_to_string(&path)? == raw);
        ensure!(std::fs::read_dir(directory.path())?.count() == 1);

        // CLI context layers must not reintroduce the connection credentials.
        for human in [false, true] {
            let cli_directory = tempfile::tempdir()?;
            let mut cli = command(cli_directory.path());
            if human {
                cli.arg("--human");
            }
            let output = tokio::time::timeout(
                Duration::from_secs(20),
                cli.args(["key", "sign", "--user", "fixture", "--db", &url])
                    .output(),
            )
            .await??;
            ensure!(!output.status.success());
            if !human {
                let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                ensure!(
                    report["error"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains(feature)),
                    "missing structured driver diagnostic"
                );
            }
            let diagnostic = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            ensure!(diagnostic.contains(feature), "missing driver diagnostic");
            ensure!(
                !diagnostic.contains("private-password"),
                "credential in CLI diagnostic"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn unavailable_ingress_rejects_before_assembly_reload_or_preflight_effects() -> Result<()> {
    let mut cases: Vec<(String, &str)> = Vec::new();
    if !cfg!(feature = "server-tools") {
        for selection in [
            "  mcp_servers: [fixture]\n",
            "  advisor: true\n",
            "  subagent: true\n",
            "  fusion: {}\n",
            "  web_search: {}\n",
            "  web_fetch: {}\n",
        ] {
            cases.push((
                format!("server_tools:\n{selection}"),
                "server-tools feature",
            ));
        }
    }
    if !cfg!(feature = "mcp") {
        cases.push((
            "mcp_servers:\n  fixture:\n    name: fixture\n    transport:\n      type: http\n      url: 'http://127.0.0.1:1/mcp'\n".to_owned(),
            "mcp feature",
        ));
    }
    if !cfg!(feature = "harness") {
        cases.push((
            "agent_api:\n  enabled: true\n".to_owned(),
            "harness feature",
        ));
    }
    for (activation, expected) in cases {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bitrouter.yaml");
        let raw = format!(
            "inherit_defaults: false\nregistry:\n  enabled: false\nserver:\n  listen: '127.0.0.1:0'\ndatabase:\n  url: 'sqlite://must-not-create.db?mode=rwc'\n{activation}"
        );
        std::fs::write(&path, &raw)?;
        let source = ConfigSource::File(path.clone());
        let config = bitrouter::config::parse_with(&raw, |_| None)?;
        let assembly = bitrouter::build_app_with_path(&config, Some(&path))
            .await
            .err()
            .context("assembly accepted an unavailable ingress")?;
        let reload = bitrouter::reload::load_configuration_baseline(&source)
            .await
            .err()
            .context("reload accepted an unavailable ingress")?;
        let preflight = bitrouter::upgrade_preflight::Preflight::check_explicit(&source)
            .await
            .err()
            .context("preflight accepted an unavailable ingress")?;
        for error in [assembly, reload, preflight] {
            ensure!(format!("{error:#}").contains(expected), "{error:#}");
        }
        ensure!(std::fs::read_to_string(&path)? == raw);
        ensure!(std::fs::read_dir(directory.path())?.count() == 1);
    }
    Ok(())
}

#[tokio::test]
async fn unavailable_ingress_commands_reject_before_configuration_or_logs() -> Result<()> {
    let mut cases: Vec<(&[&str], &str)> = Vec::new();
    if !cfg!(feature = "tui") {
        cases.extend([
            (&["code", "--config", "missing.yaml"][..], "tui feature"),
            (&["tui", "--config", "missing.yaml"][..], "tui feature"),
            (&["agents"][..], "tui feature"),
            (
                &["agents", "attach", "fixture", "--config", "missing.yaml"][..],
                "tui feature",
            ),
            (
                &[
                    "init",
                    "--yes",
                    "--after",
                    "launch",
                    "--config",
                    "missing.yaml",
                ][..],
                "tui feature",
            ),
        ]);
    }
    if !cfg!(feature = "mcp") {
        cases.extend([
            (
                &["mcp", "check", "--config", "missing.yaml"][..],
                "mcp feature",
            ),
            (
                &["tools", "list", "--config", "missing.yaml"][..],
                "mcp feature",
            ),
            (
                &["tools", "status", "--config", "missing.yaml"][..],
                "mcp feature",
            ),
            (
                &["tools", "discover", "fixture", "--config", "missing.yaml"][..],
                "mcp feature",
            ),
        ]);
    }
    if !cfg!(feature = "harness") {
        cases.push((
            &["acp", "serve", "--config", "missing.yaml"],
            "harness feature",
        ));
    }
    if !cfg!(all(feature = "core", feature = "harness")) {
        cases.extend([
            (
                &["task", "run", "hello", "--config", "missing.yaml"][..],
                "core feature and harness feature",
            ),
            (
                &["task", "managed", "hello", "--config", "missing.yaml"][..],
                "core feature and harness feature",
            ),
        ]);
    }
    for (arguments, expected) in cases {
        let directory = tempfile::tempdir()?;
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            command(directory.path()).args(arguments).output(),
        )
        .await??;
        ensure!(!output.status.success());
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(diagnostic.contains(expected), "{diagnostic}");
        ensure!(std::fs::read_dir(directory.path())?.count() == 0);
    }
    Ok(())
}

#[tokio::test]
async fn gateway_serves_inference_with_only_its_enabled_local_listeners() -> Result<()> {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "fixture-reply", "object": "chat.completion", "created": 0,
            "model": "test-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ready"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&upstream)
        .await;
    let port_reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let listen = port_reservation.local_addr()?;
    drop(port_reservation);
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("bitrouter.yaml");
    let mut raw = format!(
        "inherit_defaults: false\nregistry:\n  enabled: false\nserver:\n  listen: '{listen}'\n  skip_auth: true\ndatabase:\n  url: 'sqlite://gateway.db?mode=rwc'\nproviders:\n  fixture:\n    api_base: '{}'\n    api_key: fixture-secret\n    api_protocol:\n      - '*': chat_completions\n    models:\n      - id: test-model\n        pricing:\n          input_micro_usd_per_token: 2.0\n          output_micro_usd_per_token: 10.0\n",
        upstream.uri()
    );
    // Tuning without any selected server tool is dormant in every profile.
    raw.push_str("server_tools:\n  max_iterations: 7\n");
    if !cfg!(feature = "harness") {
        // An enabled harness rejects this nonexistent workspace. A gateway
        // must neither discover nor reserve it while retaining saved settings.
        raw.push_str("agent_api:\n  enabled: false\n  workspaces: ['missing-workspace']\n");
    }
    std::fs::write(&config_path, &raw)?;
    let config = bitrouter::config::parse_with(&raw, |_| None)?;
    let source = ConfigSource::File(config_path.clone());
    // Seed the persisted v1 filename before the process starts. Its digest is
    // the logical configuration path, with only the parent canonicalized.
    use sha2::{Digest, Sha256};
    let logical_config = std::fs::canonicalize(directory.path())?.join("bitrouter.yaml");
    #[cfg(unix)]
    let identity = {
        use std::os::unix::ffi::OsStrExt;
        hex::encode(Sha256::digest(logical_config.as_os_str().as_bytes()))
    };
    #[cfg(windows)]
    let identity = hex::encode(Sha256::digest(
        logical_config
            .to_string_lossy()
            .to_ascii_lowercase()
            .as_bytes(),
    ));
    let supervisor_ledger = source
        .home()
        .join(format!("supervisor-runs-v1-{identity}.json"));
    let dormant_ledger = b"unavailable harness data must not be decoded or rewritten";
    let socket = daemon::socket_path_for(&source, &config);
    let endpoints = [
        socket.with_extension("agent.sock"),
        socket.with_extension("acp.sock"),
    ];
    if !cfg!(feature = "harness") {
        std::fs::write(&supervisor_ledger, dormant_ledger)?;
        for endpoint in &endpoints {
            std::fs::write(endpoint, "not a harness endpoint")?;
        }
    }
    let mut process = command(directory.path())
        .args(["serve", "--config"])
        .arg(&config_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            ensure!(
                process.try_wait()?.is_none(),
                "gateway exited before readiness"
            );
            if let Ok(DaemonResponse::Status { .. }) =
                daemon::send_command(&socket, &DaemonCommand::Status).await
            {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    if !cfg!(feature = "harness") {
        let status = daemon::send_command(&socket, &DaemonCommand::Status).await?;
        let DaemonResponse::Status {
            handoff_activity: Some(activity),
            ..
        } = status
        else {
            anyhow::bail!("gateway did not report handoff activity");
        };
        ensure!(activity.supervised_runs == Some(0));
        ensure!(activity.route_leases == 0);
        let commands = [
            DaemonCommand::AcpRouteList {
                api_principal: "local".into(),
                controller_instance_id: "fixture".into(),
                session_id: "fixture".into(),
            },
            DaemonCommand::AcpRouteSet {
                api_principal: "local".into(),
                controller_instance_id: "fixture".into(),
                session_id: "fixture".into(),
                route: "test-model".into(),
            },
            DaemonCommand::AcpRouteReset {
                api_principal: "local".into(),
                controller_instance_id: "fixture".into(),
                session_id: "fixture".into(),
            },
            DaemonCommand::AcpControllerCleanup {
                api_principal: "local".into(),
                controller_instance_id: "fixture".into(),
            },
            serde_json::from_value(serde_json::json!({
                "cmd": "sessions", "request": {"version": 1, "command": {"op": "list"}}
            }))?,
        ];
        for command in commands {
            let response = daemon::send_command(&socket, &command).await?;
            let DaemonResponse::Error { message } = response else {
                anyhow::bail!("unavailable harness command was accepted");
            };
            ensure!(message.contains("harness feature"), "{message}");
        }

        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        // The no-harness control limit includes the newline. Test the real
        // listener at the boundary and without EOF/newline on an oversized
        // session payload; the latter must not wait for the peer to close.
        const LIMIT: usize = 4 * 1024 * 1024;
        let mut exact = br#"{"cmd":"status"}"#.to_vec();
        exact.resize(LIMIT - 1, b' ');
        exact.push(b'\n');
        let mut over = br#"{"cmd":"status"}"#.to_vec();
        over.resize(LIMIT, b' ');
        over.push(b'\n');
        let mut unterminated =
            br#"{"cmd":"sessions","request":{"authorization":"private-proof","padding":""#.to_vec();
        unterminated.resize(LIMIT + 1, b'x');
        for (payload, accepted) in [(exact, true), (over, false), (unterminated, false)] {
            let response: DaemonResponse = tokio::time::timeout(Duration::from_secs(20), async {
                let mut stream = daemon::connect_control(&socket).await?;
                stream.write_all(&payload).await?;
                let mut reader = BufReader::new(stream).take(64 * 1024);
                let mut line = String::new();
                reader.read_line(&mut line).await?;
                Ok::<_, anyhow::Error>(serde_json::from_str(&line)?)
            })
            .await??;
            if accepted {
                ensure!(matches!(response, DaemonResponse::Status { .. }));
            } else {
                let DaemonResponse::Error { message } = response else {
                    anyhow::bail!("oversized local control request was accepted");
                };
                ensure!(message == "local control command exceeds 4 MiB");
            }
        }
    }
    let client = reqwest::Client::new();
    ensure!(
        client
            .get(format!("http://{listen}/health"))
            .send()
            .await?
            .status()
            .is_success()
    );
    let response = client
        .post(format!("http://{listen}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "test-model", "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await?;
    ensure!(status.is_success(), "{status}: {body}");
    ensure!(
        body["choices"][0]["message"]["content"] == "ready",
        "{body}"
    );
    if !cfg!(feature = "harness") {
        for endpoint in &endpoints {
            ensure!(std::fs::read_to_string(endpoint)? == "not a harness endpoint");
            ensure!(!endpoint.with_extension("sock.lock").exists());
        }
    }
    #[cfg(all(unix, feature = "harness"))]
    for endpoint in &endpoints {
        let connection = tokio::net::UnixStream::connect(endpoint).await?;
        drop(connection);
    }
    ensure!(matches!(
        daemon::send_command(&socket, &DaemonCommand::Stop).await?,
        DaemonResponse::Ok
    ));
    let status = tokio::time::timeout(Duration::from_secs(20), process.wait()).await??;
    ensure!(status.success(), "gateway shutdown failed: {status}");
    ensure!(
        upstream
            .received_requests()
            .await
            .context("missing upstream calls")?
            .len()
            == 1
    );
    #[cfg(unix)]
    ensure!(!socket.exists());
    if cfg!(feature = "harness") {
        #[cfg(unix)]
        for endpoint in &endpoints {
            ensure!(!endpoint.exists());
        }
    } else {
        ensure!(std::fs::read(&supervisor_ledger)? == dormant_ledger);
        ensure!(!supervisor_ledger.with_extension("json.tmp").exists());
    }
    Ok(())
}
