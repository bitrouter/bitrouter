//! Saved configuration must not impersonate the daemon's running state.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::actions::administration::Administration;
use bitrouter::actions::status::{DaemonStatus, StatusReport};
use bitrouter::daemon::{self, DaemonCommand, DaemonReloader, DaemonResponse};
use bitrouter::paths::ConfigSource;
use bitrouter::reload::{AppReloader, ReloadSource};
use serde_json::Value;

struct RunningFixture {
    directory: tempfile::TempDir,
    source: PathBuf,
    socket: PathBuf,
    reloader: Arc<AppReloader>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for RunningFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn yaml(listen: &str, key: &str) -> String {
    format!(
        "inherit_defaults: false\nserver:\n  listen: '{listen}'\n  skip_auth: true\ndatabase:\n  url: 'sqlite::memory:'\nproviders:\n  fixture:\n    api_base: https://example.invalid/v1\n    api_key: {key}\n    active: true\n    models: [{{id: fixture-model}}]\n  claude-code:\n    active: false\n"
    )
}

impl RunningFixture {
    async fn start() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("bitrouter.yaml");
        let socket = directory.path().join("bitrouter.sock");
        tokio::fs::write(&source, yaml("127.0.0.1:14356", "old-private-api-key")).await?;
        let baseline =
            bitrouter::reload::load_configuration_baseline(&ConfigSource::File(source.clone()))
                .await?;
        let assembled = bitrouter::build_app_with_path(baseline.config(), Some(&source)).await?;
        let reloader = Arc::new(
            AppReloader::new(
                assembled.policy_store.clone(),
                assembled.routing_table.clone(),
                assembled.upstream_executor.clone(),
                ReloadSource::File(source.clone()),
            )
            .with_startup_configuration(baseline)
            .with_policy_runtime(assembled.policy_runtime.clone())
            .with_policy_table_router(assembled.policy_table_router.clone()),
        );
        let administration = Administration {
            source: ConfigSource::File(source.clone()),
            routing: assembled.routing_table.clone(),
            policy: assembled.policy_runtime.clone(),
            observe: assembled.observe.clone(),
        };
        let task = tokio::spawn(
            daemon::run_control_socket_with_acp_runtime_and_administration(
                socket.clone(),
                Arc::new(assembled.app),
                "127.0.0.1:14356".into(),
                reloader.clone(),
                assembled.observe,
                daemon::AcpControlPlane {
                    runtime: assembled.acp_runtime,
                    metering: bitrouter::metering::MeteringStore::new(assembled.db),
                    inventory: None,
                    evolution: None,
                },
                Some(administration),
            ),
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    daemon::send_command(&socket, &DaemonCommand::Status).await,
                    Ok(DaemonResponse::Status { .. })
                ) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("control socket did not become ready")?;
        Ok(Self {
            directory,
            source,
            socket,
            reloader,
            task,
        })
    }

    async fn status(&self, source: &Path) -> Result<StatusReport> {
        DaemonStatus::new(&self.socket, Some(ConfigSource::File(source.to_owned())))
            .report()
            .await
    }
}

fn configuration(report: &StatusReport) -> Result<Value> {
    let report = serde_json::to_value(report)?;
    let state = report
        .get("config_state")
        .context("daemon omitted config state")?;
    Ok(state.clone())
}

#[tokio::test]
async fn saved_edits_reload_and_restart_have_target_owned_evidence() -> Result<()> {
    let fixture = RunningFixture::start().await?;
    let initial = fixture.status(&fixture.source).await?;
    let initial_state = configuration(&initial)?;
    ensure!(
        initial_state["running"] == "in_sync",
        "initial: {initial_state}"
    );

    tokio::fs::write(
        &fixture.source,
        format!(
            "# operator comment only\n{}",
            yaml("127.0.0.1:14356", "old-private-api-key")
        ),
    )
    .await?;
    let unchanged = configuration(&fixture.status(&fixture.source).await?)?;
    ensure!(
        unchanged["running"] == "in_sync",
        "comment-only edit: {unchanged}"
    );

    tokio::fs::write(
        &fixture.source,
        yaml("127.0.0.1:14356", "new-private-api-key"),
    )
    .await?;
    let pending = fixture.status(&fixture.source).await?;
    let pending_state = configuration(&pending)?;
    ensure!(
        pending_state["running"] == "reload_required",
        "pending: {pending_state}"
    );
    ensure!(
        pending_state["reload_required_fields"]
            .as_array()
            .is_some_and(|fields| fields.contains(&Value::String("providers".into())))
    );
    let serialized = serde_json::to_string(&pending)?;
    ensure!(
        !serialized.contains("old-private-api-key") && !serialized.contains("new-private-api-key")
    );

    fixture
        .reloader
        .reload()
        .await
        .with_context(|| format!("reload evidence: {:?}", fixture.reloader.reload_state()))?;
    let applied = configuration(&fixture.status(&fixture.source).await?)?;
    ensure!(applied["running"] == "in_sync", "after reload: {applied}");

    tokio::fs::write(
        &fixture.source,
        yaml("127.0.0.1:14357", "new-private-api-key"),
    )
    .await?;
    let saved = fixture.status(&fixture.source).await?;
    let saved_state = configuration(&saved)?;
    ensure!(
        saved_state["running"] == "restart_required",
        "restart: {saved_state}"
    );
    ensure!(saved.listen.as_deref() == Some("127.0.0.1:14356"));
    ensure!(fixture.reloader.reload().await.is_err());
    let refused = fixture.status(&fixture.source).await?;
    ensure!(configuration(&refused)?["running"] == "restart_required");
    ensure!(refused.listen.as_deref() == Some("127.0.0.1:14356"));

    let alternate = fixture.directory.path().join("different-config.yaml");
    tokio::fs::write(&alternate, "inherit_defaults: false\nrouters:\n  unrelated:\n    selection:\n      kind: model\n      model: nowhere:model\n").await?;
    let same_target = fixture.status(&alternate).await?;
    ensure!(configuration(&same_target)?["running"] == "restart_required");
    ensure!(!serde_json::to_string(&same_target)?.contains("unrelated"));
    Ok(())
}

#[tokio::test]
async fn invalid_or_missing_saved_config_does_not_erase_the_live_daemon() -> Result<()> {
    let fixture = RunningFixture::start().await?;
    tokio::fs::write(&fixture.source, "routers: [invalid\n").await?;
    let invalid = fixture.status(&fixture.source).await?;
    ensure!(invalid.running);
    ensure!(invalid.listen.as_deref() == Some("127.0.0.1:14356"));
    let invalid_state = configuration(&invalid)?;
    ensure!(
        invalid_state["saved"] == "invalid",
        "invalid: {invalid_state}"
    );
    ensure!(invalid_state["running"] != "in_sync");

    tokio::fs::remove_file(&fixture.source).await?;
    let missing = fixture.status(&fixture.source).await?;
    ensure!(missing.running);
    let missing_state = configuration(&missing)?;
    ensure!(
        missing_state["saved"] == "missing",
        "missing: {missing_state}"
    );
    ensure!(missing_state["running"] != "in_sync");
    tokio::fs::create_dir(&fixture.source).await?;
    let unreadable = fixture.status(&fixture.source).await?;
    ensure!(unreadable.running);
    let unreadable_state = configuration(&unreadable)?;
    ensure!(
        unreadable_state["saved"] == "unavailable",
        "{unreadable_state}"
    );
    ensure!(unreadable_state["running"] == "unknown");
    Ok(())
}

#[tokio::test]
async fn unknown_field_diagnostics_do_not_disclose_dynamic_keys_or_values() -> Result<()> {
    let fixture = RunningFixture::start().await?;
    let candidate = format!(
        "{}\nprivate-unrecognized-key: private-unrecognized-value\n",
        yaml("127.0.0.1:14356", "old-private-api-key")
    );
    tokio::fs::write(&fixture.source, candidate).await?;
    ensure!(fixture.reloader.reload().await.is_err());
    let report = fixture.status(&fixture.source).await?;
    let state = configuration(&report)?;
    ensure!(state["running"] == "restart_required", "{state}");
    ensure!(state["restart_required_fields"] == serde_json::json!(["unclassified"]));
    let serialized = serde_json::to_string(&report)?;
    ensure!(!serialized.contains("private-unrecognized"));
    ensure!(!serialized.contains("old-private-api-key"));
    Ok(())
}

fn isolated_cli(home: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("BITROUTER_HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    for variable in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
        "OPENCODE_ZEN_API_KEY",
        "BITROUTER_API_KEY",
    ] {
        command.env_remove(variable);
    }
    command
}

async fn cli_status(home: &Path, source: &Path) -> Result<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        isolated_cli(home)
            .arg("status")
            .arg("-c")
            .arg(source)
            .output(),
    )
    .await??;
    ensure!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("status JSON")
}

#[tokio::test]
async fn cli_finds_live_daemon_after_socket_edit_and_source_deletion() -> Result<()> {
    #[cfg(unix)]
    let temporary_root = PathBuf::from("/tmp");
    #[cfg(not(unix))]
    let temporary_root = std::env::temp_dir();
    let directory = tempfile::Builder::new()
        .prefix("bro-state-")
        .tempdir_in(temporary_root)?;
    let home = directory.path();
    let source = home.join("bitrouter.yaml");
    let original = "inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\n  control_socket: old.sock\ndatabase:\n  url: 'sqlite::memory:'\n";
    tokio::fs::write(&source, original).await?;
    let mut child = isolated_cli(home)
        .arg("serve")
        .arg("-c")
        .arg(&source)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let initial = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("daemon exited before readiness: {status}");
            }
            if let Ok(report) = cli_status(home, &source).await
                && report["running"] == true
            {
                break Ok::<_, anyhow::Error>(report);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    ensure!(
        initial["config_state"]["running"] == "in_sync",
        "initial: {initial}"
    );
    let instance = initial["config_state"]["server_instance_id"].clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut entries = tokio::fs::read_dir(home).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(".bitrouter-daemon-") && name.ends_with(".json") {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;

    tokio::fs::write(&source, original.replace("old.sock", "new.sock")).await?;
    let changed = cli_status(home, &source).await?;
    ensure!(
        changed["running"] == true,
        "edited socket lost daemon: {changed}"
    );
    ensure!(changed["config_state"]["server_instance_id"] == instance);
    ensure!(changed["config_state"]["running"] == "restart_required");
    ensure!(!home.join("new.sock").exists());

    tokio::fs::write(&source, "server: [broken\n").await?;
    let invalid = cli_status(home, &source).await?;
    ensure!(invalid["running"] == true);
    ensure!(invalid["config_state"]["saved"] == "invalid");
    tokio::fs::remove_file(&source).await?;
    let missing = cli_status(home, &source).await?;
    ensure!(missing["running"] == true);
    ensure!(missing["config_state"]["saved"] == "missing");
    ensure!(missing["config_state"]["server_instance_id"] == instance);

    let stopped = isolated_cli(home)
        .arg("stop")
        .arg("-c")
        .arg(&source)
        .output()
        .await?;
    ensure!(
        stopped.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let exit = tokio::time::timeout(Duration::from_secs(10), child.wait()).await??;
    ensure!(exit.success());
    let after = cli_status(home, &source).await?;
    ensure!(after["running"] == false);
    ensure!(after["config_state"]["running"] == "unknown");
    Ok(())
}
