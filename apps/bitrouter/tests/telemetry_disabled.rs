//! Export-unavailable artifacts reject activation before service or account effects.

#![cfg(not(any(feature = "otel-http", feature = "otel-grpc")))]

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::paths::ConfigSource;

fn configuration(telemetry: &str) -> String {
    format!(
        "inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\ndatabase:\n  url: 'sqlite://must-not-create.db?mode=rwc'\nplugins:\n  bitrouter-telemetry:\n{telemetry}\n"
    )
}

#[tokio::test]
async fn startup_reload_loading_and_handoff_preflight_reject_before_storage() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bitrouter.yaml");
    let raw = configuration("    telemetry: { enabled: true, attribution: account }");
    std::fs::write(&path, &raw)?;
    let source = ConfigSource::File(path.clone());

    let config = bitrouter::config::parse_with(&raw, |_| None)?;
    let assembly_error = bitrouter::build_app_with_path(&config, Some(&path))
        .await
        .err()
        .context("assembly accepted unavailable export")?;
    let load_error = bitrouter::paths::load_config(&source)
        .await
        .err()
        .context("configuration load accepted unavailable export")?;
    let reload_error = bitrouter::reload::load_configuration_baseline(&source)
        .await
        .err()
        .context("reload preparation accepted unavailable export")?;
    let preflight_error = bitrouter::upgrade_preflight::Preflight::check_explicit(&source)
        .await
        .err()
        .context("handoff preflight accepted unavailable export")?;
    for error in [assembly_error, load_error, reload_error, preflight_error] {
        let diagnostic = format!("{error:#}");
        ensure!(
            diagnostic.contains("plugins.bitrouter-telemetry.telemetry.enabled"),
            "{diagnostic}"
        );
        ensure!(
            diagnostic.contains("otel-http or otel-grpc"),
            "{diagnostic}"
        );
    }
    ensure!(std::fs::read_to_string(&path)? == raw);
    ensure!(std::fs::read_dir(directory.path())?.count() == 1);
    Ok(())
}

#[tokio::test]
async fn explicit_yaml_and_environment_opt_in_fail_before_serve_side_effects() -> Result<()> {
    for (telemetry, endpoint, diagnostic_path) in [
        (
            "    telemetry: { enabled: true, attribution: account }",
            None,
            "plugins.bitrouter-telemetry.telemetry.enabled",
        ),
        (
            "    otel: { endpoint: 'http://127.0.0.1:1', bearer_token: private-token }",
            None,
            "plugins.bitrouter-telemetry.otel",
        ),
        (
            "    telemetry: { enabled: false }",
            Some("http://127.0.0.1:1"),
            "OTEL_EXPORTER_OTLP_ENDPOINT",
        ),
    ] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bitrouter.yaml");
        std::fs::write(&path, configuration(telemetry))?;
        let runtime = directory.path().join("runtime");
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .args(["serve", "--config"])
            .arg(&path)
            .current_dir(directory.path())
            .env_clear()
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env("BITROUTER_HOME", &runtime)
            .kill_on_drop(true);
        if let Some(endpoint) = endpoint {
            command.env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint);
        }
        let output = tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .context("serve did not reject unavailable export")??;
        ensure!(!output.status.success());
        // Noninteractive CLI failures use the public JSON error envelope on stdout.
        let stdout: serde_json::Value = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("CLI stderr: {}", String::from_utf8_lossy(&output.stderr)))?;
        let diagnostic = stdout["error"]["message"]
            .as_str()
            .context("missing CLI error message")?;
        ensure!(diagnostic.contains(diagnostic_path), "{diagnostic}");
        ensure!(
            diagnostic.contains("otel-http or otel-grpc"),
            "{diagnostic}"
        );
        ensure!(!diagnostic.contains("private-token"), "{diagnostic}");
        ensure!(
            !runtime.exists(),
            "serve touched runtime home before rejecting export"
        );
        ensure!(std::fs::read_dir(directory.path())?.count() == 1);
    }
    Ok(())
}

#[tokio::test]
async fn zero_config_environment_opt_in_rejects_before_enrichment() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let runtime = directory.path().join(".bitrouter");
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"))
            .arg("serve")
            .current_dir(directory.path())
            .env_clear()
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("zero-config serve did not reject unavailable export")??;
    ensure!(!output.status.success());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let diagnostic = stdout["error"]["message"]
        .as_str()
        .context("missing CLI error message")?;
    ensure!(
        diagnostic.contains("OTEL_EXPORTER_OTLP_ENDPOINT"),
        "{diagnostic}"
    );
    ensure!(
        diagnostic.contains("otel-http or otel-grpc"),
        "{diagnostic}"
    );
    // Startup may create the empty home before loading defaults, but must not
    // initialize telemetry notice/installation identity, credentials or storage.
    if runtime.exists() {
        ensure!(std::fs::read_dir(&runtime)?.count() == 0);
    }
    Ok(())
}

#[tokio::test]
async fn dormant_export_configuration_remains_readable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bitrouter.yaml");
    let raw = configuration(
        "    telemetry: { enabled: false, level: full, attribution: account, endpoint: 'http://collector:4318' }",
    );
    std::fs::write(&path, &raw)?;
    let source = ConfigSource::File(path.clone());
    let config = bitrouter::paths::load_config(&source).await?;
    let baseline = bitrouter::reload::load_configuration_baseline(&source).await?;
    ensure!(config.plugins == baseline.config().plugins);
    ensure!(config.plugins["bitrouter-telemetry"]["telemetry"]["level"] == "full");
    ensure!(std::fs::read_to_string(&path)? == raw);
    ensure!(std::fs::read_dir(directory.path())?.count() == 1);
    Ok(())
}

#[cfg(all(unix, feature = "harness", feature = "tui"))]
#[test]
fn interactive_chat_child() -> Result<()> {
    use std::io::IsTerminal;

    let Some(home) = std::env::var_os("BITROUTER_TEST_TELEMETRY_CHAT_HOME") else {
        return Ok(());
    };
    ensure!(std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
    let source = ConfigSource::Default { home: home.into() };
    let mut config = bitrouter::config::Config::default();
    config.plugins.insert(
        "bitrouter-telemetry".into(),
        serde_json::json!({
            "telemetry": { "enabled": true, "attribution": "account" }
        }),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let error = runtime
        .block_on(bitrouter::acp_cli::chat(bitrouter::acp_cli::SpawnContext {
            source: &source,
            config,
            agent_id: "unconfigured-agent",
            options: bitrouter::acp_cli::LaunchOptions::default(),
            routing: bitrouter::commands::agent_options::RoutingOptions::default(),
        }))
        .err()
        .context("interactive chat accepted unavailable export")?;
    ensure!(
        error.to_string().contains("otel-http or otel-grpc"),
        "{error:#}"
    );
    ensure!(std::fs::read_dir(source.home())?.count() == 0);
    println!("CHAT_EXPORT_REJECTED_BEFORE_TERMINAL_DISPATCH");
    Ok(())
}

#[cfg(all(unix, feature = "harness", feature = "tui"))]
#[test]
fn interactive_public_chat_rejects_before_terminal_dispatch() -> Result<()> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;
    use std::time::Instant;

    let directory = tempfile::tempdir()?;
    let pty = native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let reader = pty.master.try_clone_reader()?;
    let mut command = CommandBuilder::new(std::env::current_exe()?);
    command.args(["--exact", "interactive_chat_child", "--nocapture"]);
    command.env_clear();
    command.env("HOME", directory.path());
    command.env("BITROUTER_HOME", directory.path());
    command.env("BITROUTER_TEST_TELEMETRY_CHAT_HOME", directory.path());
    command.cwd(directory.path());
    let mut child = pty.slave.spawn_command(command)?;
    drop(pty.slave);
    let result = (|| -> Result<()> {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("telemetry-chat-pty".into())
            .spawn(move || {
                let mut output = Vec::new();
                let read = reader.take(64 * 1024).read_to_end(&mut output);
                let _ = sender.send((read, output));
            })?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            ensure!(
                Instant::now() < deadline,
                "interactive chat did not reject export"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let (read, output) = receiver.recv_timeout(Duration::from_secs(5))?;
        read?;
        let output = String::from_utf8(output)?;
        ensure!(status.success(), "{output}");
        ensure!(
            output.contains("CHAT_EXPORT_REJECTED_BEFORE_TERMINAL_DISPATCH"),
            "{output}"
        );
        ensure!(
            !output.contains("\u{1b}[?1049h"),
            "terminal drawing started before rejection"
        );
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}
