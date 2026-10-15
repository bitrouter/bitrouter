//! Learning commands reject unavailable execution before opening inputs or stores.

#![cfg(not(feature = "core"))]

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};

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

async fn rejection(command: &mut tokio::process::Command) -> Result<String> {
    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("unavailable operation did not return")??;
    ensure!(!output.status.success(), "unavailable operation succeeded");
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("CLI stderr: {}", String::from_utf8_lossy(&output.stderr)))?;
    let message = envelope["error"]["message"]
        .as_str()
        .context("missing CLI error message")?;
    ensure!(message.contains("core feature"), "{message}");
    Ok(message.to_owned())
}

#[tokio::test]
async fn learning_commands_reject_before_input_or_storage_access() -> Result<()> {
    let cases: &[&[&str]] = &[
        &["eval", "status", "--config", "missing.yaml"],
        &[
            "eval",
            "subject",
            "seal",
            "missing.json",
            "--output",
            "sealed.json",
        ],
        &["optimize", "run", "--config", "missing.yaml"],
        &["optimize", "status", "--config", "missing.yaml"],
        &[
            "workflow-state",
            "classifier-bakeoff",
            "--fixtures",
            "missing",
            "--output",
            "report.json",
        ],
        &[
            "policy",
            "compile",
            "--config",
            "missing.yaml",
            "--output",
            "candidate.yaml",
        ],
        &["policy", "verify", "--evidence", "--config", "missing.yaml"],
        &["policy", "evolve", "--apply", "--config", "missing.yaml"],
        &[
            "policy",
            "publish",
            "missing.yaml",
            "--config",
            "missing.yaml",
        ],
    ];
    for arguments in cases {
        let directory = tempfile::tempdir()?;
        rejection(command(directory.path()).args(*arguments)).await?;
        ensure!(
            std::fs::read_dir(directory.path())?.count() == 0,
            "{arguments:?} created state before rejecting unavailable execution"
        );
    }
    Ok(())
}

#[tokio::test]
async fn workflow_trace_activation_rejects_before_launcher_side_effects() -> Result<()> {
    for action in ["serve", "start", "restart"] {
        for explicit_config in [false, true] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("bitrouter.yaml");
            let trace = directory.path().join("workflow.jsonl");
            let mut command = command(directory.path());
            command
                .arg(action)
                .env("BITROUTER_WORKFLOW_TRACE_JSONL", &trace);
            if !explicit_config {
                // BITROUTER_HOME selects an explicit configuration directory; omit
                // it to exercise the existing zero-config path under the test HOME.
                command.env_remove("BITROUTER_HOME");
            }
            if explicit_config {
                std::fs::write(
                    &path,
                    "inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\ndatabase:\n  url: 'sqlite://must-not-create.db?mode=rwc'\n",
                )?;
                command.arg("--config").arg(&path);
            }
            let diagnostic = rejection(&mut command).await?;
            ensure!(diagnostic.contains("BITROUTER_WORKFLOW_TRACE_JSONL"));
            ensure!(!trace.exists());
            ensure!(!directory.path().join("runtime").exists());
            ensure!(std::fs::read_dir(directory.path())?.count() == usize::from(explicit_config));
        }
    }
    Ok(())
}

#[tokio::test]
async fn unsupported_trace_restart_leaves_the_resident_daemon_running() -> Result<()> {
    use bitrouter::daemon::{self, DaemonCommand, DaemonResponse};

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bitrouter.yaml");
    let raw = "inherit_defaults: false\nregistry:\n  enabled: false\nserver:\n  listen: '127.0.0.1:0'\ndatabase:\n  url: 'sqlite://daemon.db?mode=rwc'\n";
    std::fs::write(&path, raw)?;
    let config = bitrouter::config::parse_with(raw, |_| None)?;
    let source = bitrouter::paths::ConfigSource::File(path.clone());
    let socket = daemon::socket_path_for(&source, &config);
    let mut resident = command(directory.path())
        .args(["serve", "--config"])
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let resident_pid = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            ensure!(
                resident.try_wait()?.is_none(),
                "resident exited before readiness"
            );
            if let Ok(DaemonResponse::Status { pid, .. }) =
                daemon::send_command(&socket, &DaemonCommand::Status).await
            {
                return Ok::<_, anyhow::Error>(pid);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("resident control endpoint never became ready")??;

    let trace = directory.path().join("workflow.jsonl");
    rejection(
        command(directory.path())
            .args(["restart", "--config"])
            .arg(&path)
            .env("BITROUTER_WORKFLOW_TRACE_JSONL", &trace),
    )
    .await?;
    ensure!(
        resident.try_wait()?.is_none(),
        "restart stopped the resident"
    );
    let status = daemon::send_command(&socket, &DaemonCommand::Status).await?;
    ensure!(matches!(status, DaemonResponse::Status { pid, .. } if pid == resident_pid));
    ensure!(!trace.exists());

    let response = daemon::send_command(&socket, &DaemonCommand::Stop).await?;
    ensure!(matches!(response, DaemonResponse::Ok));
    let status = tokio::time::timeout(Duration::from_secs(20), resident.wait())
        .await
        .context("test-owned daemon did not shut down")??;
    ensure!(status.success());
    Ok(())
}
