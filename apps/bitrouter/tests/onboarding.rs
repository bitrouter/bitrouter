//! Binary-level coverage for the `bitrouter` onboarding front door — bare
//! invocation (wizard-vs-status decision), the `init --yes` headless contract,
//! and the network-free / `BITROUTER_HOME`-tolerant probe. Every case here is
//! hermetic: an isolated `HOME` + `XDG_DATA_HOME`, all BYOK env vars removed,
//! stdin nulled (non-TTY), and no network.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

#[cfg(unix)]
#[path = "../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
mod example_binary;

/// The BYOK env vars the probe reads — removed so the test's own environment
/// can't make an "unconfigured" case look configured.
const PROBE_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
    "OPENCODE_ZEN_API_KEY",
    "BITROUTER_API_KEY",
];

fn startup_home() -> std::io::Result<TempDir> {
    #[cfg(unix)]
    {
        tempfile::tempdir_in("/tmp")
    }
    #[cfg(not(unix))]
    {
        tempfile::tempdir()
    }
}

fn isolated_command(home: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env_remove("BITROUTER_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for var in PROBE_VARS {
        command.env_remove(var);
    }
    command
}

async fn invoke(home: &Path, args: &[&str]) -> anyhow::Result<Output> {
    Ok(tokio::time::timeout(
        std::time::Duration::from_secs(30),
        isolated_command(home).args(args).output(),
    )
    .await??)
}

struct TestDaemon {
    pid: u32,
}
impl Drop for TestDaemon {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.pid)
            && let Some(pid) = rustix::process::Pid::from_raw(pid)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .output();
        }
    }
}

fn isolated_config(home: &Path, listen: &str) -> anyhow::Result<()> {
    std::fs::write(
        home.join("bitrouter.yaml"),
        format!(
            "inherit_defaults: false\nregistry: {{ enabled: false }}\nserver: {{ listen: '{listen}', skip_auth: true }}\ndatabase: {{ url: 'sqlite::memory:' }}\n"
        ),
    )?;
    Ok(())
}

#[tokio::test]
async fn init_rejects_unpaired_keys_and_handles_a_closed_stdout_pipe() -> anyhow::Result<()> {
    let home = startup_home()?;
    let invalid = invoke(
        home.path(),
        &["init", "--yes", "--provider-api-key", "fixture-secret"],
    )
    .await?;
    assert!(!invalid.status.success());
    assert!(!String::from_utf8_lossy(&invalid.stdout).contains("fixture-secret"));
    assert!(!home.path().join(".bitrouter/bitrouter.yaml").exists());
    let mut child = isolated_command(home.path())
        .args(["init", "--yes"])
        .spawn()?;
    drop(child.stdout.take());
    let status = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await??;
    assert!(!status.success());
    assert!(home.path().join(".bitrouter/bitrouter.yaml").is_file());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn init_after_launch_explicitly_runs_the_native_harness() -> anyhow::Result<()> {
    let home = startup_home()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let listen = listener.local_addr()?.to_string();
    drop(listener);
    isolated_config(home.path(), &listen)?;
    let startup = invoke(home.path(), &[]).await?;
    let ready: Value = serde_json::from_slice(&startup.stdout)?;
    let pid = ready["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| anyhow::anyhow!("daemon not ready: {ready}"))?;
    let _daemon = TestDaemon { pid };
    let fixture =
        example_binary::build("bitrouter", "onboarding_fixture").map_err(anyhow::Error::msg)?;
    std::os::unix::fs::symlink(&fixture, home.path().join("codex"))?;
    let marker = home.path().join("native.marker");
    let path = std::env::join_paths(std::iter::once(home.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        isolated_command(home.path())
            .args(["init", "--yes", "--harness", "codex", "--after", "launch"])
            .env("PATH", path)
            .env("ONBOARDING_NATIVE_MARKER", &marker)
            .output(),
    )
    .await??;
    assert!(
        output.status.success(),
        "{}; {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(marker)?, "native codex launched");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?["after"],
        "launch"
    );
    Ok(())
}

#[tokio::test]
async fn bare_and_init_ensure_one_ready_background_daemon_and_preserve_running_settings()
-> anyhow::Result<()> {
    let home = startup_home()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let listen = listener.local_addr()?.to_string();
    drop(listener);
    isolated_config(home.path(), &listen)?;
    let (first, second) = tokio::join!(invoke(home.path(), &[]), invoke(home.path(), &[]));
    let first = first?;
    let second = second?;
    let a: Value = serde_json::from_slice(&first.stdout)?;
    let b: Value = serde_json::from_slice(&second.stdout)?;
    let pid = a["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "startup failed: {a}; {}",
                String::from_utf8_lossy(&first.stderr)
            )
        })?;
    let _daemon = TestDaemon { pid };
    assert!(first.status.success(), "{a}");
    assert!(second.status.success(), "{b}");
    assert_eq!(a["status"], "ready");
    assert_eq!(a["pid"], b["pid"]);
    assert_eq!(a["listen"], listen);
    assert_eq!(a["config_state"]["running"], "in_sync");
    let status = invoke(home.path(), &["status"]).await?;
    assert_eq!(serde_json::from_slice::<Value>(&status.stdout)?["pid"], pid);
    let human = invoke(home.path(), &["--human"]).await?;
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("daemon ready"));
    let other = home.path().join("other");
    std::fs::create_dir_all(&other)?;
    std::fs::write(
        other.join("bitrouter.yaml"),
        serde_json::to_vec(&serde_json::json!({
            "inherit_defaults": false, "registry": {"enabled": false},
            "server": {"listen": listen, "skip_auth": true, "control_socket": home.path().join("bitrouter.sock")},
            "database": {"url": "sqlite::memory:"}
        }))?,
    )?;
    let foreign = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        isolated_command(home.path()).current_dir(&other).output(),
    )
    .await??;
    let report: Value = serde_json::from_slice(&foreign.stdout)?;
    assert!(!foreign.status.success());
    assert!(
        report["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("cannot verify")),
        "{report}"
    );

    // Saving a different socket cannot make ensure-start lose the old daemon.
    std::fs::write(
        home.path().join("bitrouter.yaml"),
        format!(
            "inherit_defaults: false\nregistry: {{ enabled: false }}\nserver: {{ listen: '{listen}', control_socket: 'new.sock', skip_auth: true }}\ndatabase: {{ url: 'sqlite::memory:' }}\n"
        ),
    )?;
    let out = invoke(home.path(), &["init", "--yes", "--after", "serve"]).await?;
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert!(out.status.success(), "{report}");
    assert_eq!(report["config_activation"], "saved_only");
    assert_eq!(report["daemon"]["pid"], pid);
    assert_eq!(
        report["daemon"]["config_state"]["running"],
        "restart_required"
    );
    assert!(!home.path().join("new.sock").exists());
    std::fs::write(home.path().join("bitrouter.yaml"), "server: [invalid]\n")?;
    let out = invoke(home.path(), &[]).await?;
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert!(out.status.success(), "{report}");
    assert_eq!(report["pid"], pid);
    assert_eq!(report["config_state"]["saved"], "invalid");
    Ok(())
}

#[tokio::test]
async fn bare_errors_on_child_exit_invalid_config_and_remote_context_without_fallback()
-> anyhow::Result<()> {
    let home = startup_home()?;
    let occupied = std::net::TcpListener::bind("127.0.0.1:0")?;
    isolated_config(home.path(), &occupied.local_addr()?.to_string())?;
    let out = invoke(home.path(), &[]).await?;
    assert!(!out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert!(
        report["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("exited during startup")),
        "{report}"
    );
    std::fs::write(home.path().join("bitrouter.yaml"), "server: [invalid]\n")?;
    let out = invoke(home.path(), &[]).await?;
    assert!(!out.status.success());
    assert!(serde_json::from_slice::<Value>(&out.stdout)?["error"].is_object());
    let out = invoke(home.path(), &["--context", "explicit-remote"]).await?;
    assert!(!out.status.success());
    assert!(
        serde_json::from_slice::<Value>(&out.stdout)?["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("remote context"))
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("bitrouter.yaml"))?,
        "server: [invalid]\n"
    );
    Ok(())
}

#[tokio::test]
async fn configured_runtime_is_reused_without_a_file_chat_agent_or_client_credentials()
-> anyhow::Result<()> {
    let home = startup_home()?;
    let runtime = home.path().join(".bitrouter");
    std::fs::create_dir_all(&runtime)?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?.to_string();
    drop(listener);
    isolated_config(&runtime, &address)?;
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        isolated_command(home.path()).current_dir(&runtime).output(),
    )
    .await??;
    let ready: Value = serde_json::from_slice(&out.stdout)?;
    let pid = ready["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| anyhow::anyhow!("daemon not ready: {ready}"))?;
    let _daemon = TestDaemon { pid };
    std::fs::rename(runtime.join("bitrouter.yaml"), runtime.join("saved.yaml"))?;
    let out = invoke(home.path(), &[]).await?;
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert!(out.status.success(), "{report}");
    assert_eq!(report["pid"], pid);
    assert_eq!(report["status"], "ready");
    assert_eq!(report["config_state"]["saved"], "missing");
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        isolated_command(home.path())
            .env("OPENAI_API_KEY", "fixture-detected-secret")
            .output(),
    )
    .await??;
    let report: Value = serde_json::from_slice(&out.stdout)?;
    assert!(out.status.success(), "{report}");
    assert_eq!(report["pid"], pid);
    assert_eq!(report["status"], "ready");
    assert_eq!(report["config_state"]["saved"], "missing");
    assert!(!runtime.join("bitrouter.yaml").exists());
    assert!(!report.to_string().contains("fixture-detected-secret"));
    Ok(())
}

/// Run the compiled binary with an isolated home/data dir and a null (non-TTY)
/// stdin. `extra_env` layers provider-key or `BITROUTER_HOME` overrides on top.
fn run_cli(home: &Path, data_home: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", data_home)
        .env_remove("BITROUTER_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in PROBE_VARS {
        command.env_remove(var);
    }
    for (k, v) in extra_env {
        command.env(k, v);
    }
    command.output().unwrap()
}

fn stdout_json(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {text}"))
}

#[test]
fn bare_unconfigured_errors_without_writing_configuration() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    let out = run_cli(home.path(), data.path(), &[], &[]);
    assert!(!out.status.success());
    assert!(
        stdout_json(&out)["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("init --yes"))
    );
    assert!(!home.path().join(".bitrouter/bitrouter.yaml").exists());
    Ok(())
}

#[test]
fn missing_explicit_bitrouter_home_requires_setup_without_falling_back() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    let selected = TempDir::new()?;
    let selected_path = selected
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("temporary home is not UTF-8"))?;
    for credentials in [false, true] {
        let mut env = vec![("BITROUTER_HOME", selected_path)];
        if credentials {
            env.push(("OPENAI_API_KEY", "fixture-detected-key"));
        }
        let out = run_cli(home.path(), data.path(), &[], &env);
        assert!(!out.status.success());
        assert!(
            stdout_json(&out)["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("init --yes"))
        );
        assert!(!selected.path().join("bitrouter.yaml").exists());
        assert!(!home.path().join(".bitrouter/bitrouter.yaml").exists());
    }
    Ok(())
}

#[test]
fn init_yes_no_creds_reports_zero_providers_and_scaffolds() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let cfg = home.path().join("bitrouter.yaml");
    // Headless with no credential flags: completes without blocking, emits the
    // envelope with zero providers, and reproduces the classic starter-file
    // scaffold at the -c path.
    let out = run_cli(
        home.path(),
        data.path(),
        &["init", "--yes", "-c", cfg.to_str().unwrap()],
        &[],
    );
    assert!(out.status.success(), "init --yes must exit 0");
    let v = stdout_json(&out);
    assert_eq!(v["action"], "onboarding");
    assert_eq!(v["providers_configured"], serde_json::json!([]));
    assert_eq!(v["harnesses_installed"], serde_json::json!([]));
    assert_eq!(v["agents_selected"], serde_json::json!([]));
    assert_eq!(v["after"], "exit");
    assert!(v["snippet"].is_null());
    assert!(cfg.exists(), "init --yes scaffolds the starter config");
    assert!(
        std::fs::read_to_string(&cfg)
            .unwrap()
            .contains("skip_auth: true"),
        "the scaffolded file is the starter config"
    );
}

#[test]
fn init_yes_cloud_login_without_key_is_reported_and_skipped() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let cfg = home.path().join("bitrouter.yaml");
    // A bare --cloud-login can't be completed by a machine (device flow), so
    // headless reports-and-skips it rather than attempting/hanging.
    let out = run_cli(
        home.path(),
        data.path(),
        &[
            "init",
            "--yes",
            "--cloud-login",
            "--after",
            "exit",
            "-c",
            cfg.to_str().unwrap(),
        ],
        &[],
    );
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(
        v["providers_skipped_interactive"],
        serde_json::json!(["bitrouter"])
    );
}

#[test]
fn init_yes_rejects_removed_workflow_optimization_before_scaffolding() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    let cfg = home.path().join("bitrouter.yaml");
    let cfg_arg = cfg
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("temporary config path is not UTF-8"))?;
    let out = run_cli(
        home.path(),
        data.path(),
        &["init", "--yes", "--optimize", "-c", cfg_arg],
        &[],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("unexpected argument '--optimize'"),
        "unexpected parser error: stdout={stdout}; stderr={stderr}"
    );
    assert!(
        !cfg.exists(),
        "removed optimization flag must fail before creating the source config"
    );
    Ok(())
}

#[test]
fn init_preserves_existing_settings_unless_force_is_requested() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    let cfg = home.path().join("bitrouter.yaml");
    std::fs::write(
        &cfg,
        "server: { listen: '127.0.0.1:9012', skip_auth: false }\n",
    )?;

    // Without --force, existing values survive the saved chat default.
    let out = run_cli(
        home.path(),
        data.path(),
        &[
            "init",
            "--yes",
            "-c",
            cfg.to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 test path"))?,
        ],
        &[],
    );
    assert!(out.status.success());
    assert!(std::fs::read_to_string(&cfg)?.contains("127.0.0.1:9012"));

    // With --force it overwrites with the starter template.
    let out = run_cli(
        home.path(),
        data.path(),
        &[
            "init",
            "--yes",
            "--force",
            "-c",
            cfg.to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 test path"))?,
        ],
        &[],
    );
    assert!(out.status.success());
    assert!(std::fs::read_to_string(&cfg)?.contains("skip_auth: true"));
    Ok(())
}

#[test]
fn init_saves_default_harness_model_and_user_home() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    let out = run_cli(
        home.path(),
        data.path(),
        &[
            "init",
            "--yes",
            "--harness",
            "codex",
            "--model",
            "openai-codex:gpt-6-astra",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = home.path().join(".bitrouter/bitrouter.yaml");
    let config: Value = serde_saphyr::from_str(&std::fs::read_to_string(&path)?)?;
    assert_eq!(config["chat"]["agent"], "codex-acp");
    assert_eq!(config["chat"]["model"], "openai-codex:gpt-6-astra");
    assert_eq!(config["server"]["listen"], "127.0.0.1:4356");
    assert_eq!(config["server"]["skip_auth"], true);
    assert!(!home.path().join("bitrouter.yaml").exists());
    Ok(())
}

#[test]
fn changing_harness_preserves_routes_and_chat_commands() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o755))?;
    }
    let path = home.path().join("bitrouter.yaml");
    std::fs::write(
        &path,
        "server: { listen: '127.0.0.1:9012', skip_auth: false }\nchat:\n  agent: codex-acp\n  model: custom/model\n  commands: [{ name: review, prompt: 'Review $ARGUMENTS' }]\nproviders: { private: { api_base: 'https://example.invalid/v1' } }\n",
    )?;
    let out = run_cli(
        home.path(),
        data.path(),
        &["init", "--yes", "--harness", "claude"],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config: Value = serde_saphyr::from_str(&std::fs::read_to_string(path)?)?;
    assert_eq!(config["chat"]["agent"], "claude-acp");
    assert_eq!(config["chat"]["model"], "custom/model");
    assert_eq!(config["chat"]["commands"][0]["name"], "review");
    assert_eq!(config["server"]["skip_auth"], false);
    assert_eq!(
        config["providers"]["private"]["api_base"],
        "https://example.invalid/v1"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(home.path())?.permissions().mode() & 0o777,
            0o755
        );
    }
    Ok(())
}

#[test]
fn public_help_exposes_canonical_agent_entry_points() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    let out = run_cli(home.path(), data.path(), &["--help"], &[]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for command in ["launch", "claude", "codex", "run", "code", "acp"] {
        assert!(help.contains(&format!("  {command}")), "{command}: {help}");
    }
    for hidden in ["spawn", "chat", "tui"] {
        assert!(
            !help.contains(&format!("  {hidden}")),
            "hidden command {hidden}: {help}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn onboarding_saves_mcp_without_installing_skills_or_starting_the_resource()
-> anyhow::Result<()> {
    let home = startup_home()?;
    isolated_config(home.path(), "127.0.0.1:0")?;
    let entry = home.path().join("mcp.yaml");
    std::fs::write(
        &entry,
        "name: review-peer\ntransport:\n  type: stdio\n  command: must-not-start\n",
    )?;
    let output = invoke(
        home.path(),
        &[
            "init",
            "--yes",
            "--mcp-name",
            "review-peer",
            "--mcp-from",
            entry
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("entry path"))?,
            "--after",
            "exit",
        ],
    )
    .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["resources"][0]["status"], "saved");
    assert_eq!(report["resources"].as_array().map(Vec::len), Some(1));
    assert!(!home.path().join(".bitrouter/skills").exists());
    Ok(())
}
#[tokio::test]
async fn onboarding_rejects_removed_installer_flags_and_keeps_mcp_failure_progress()
-> anyhow::Result<()> {
    let home = startup_home()?;
    let removed = invoke(
        home.path(),
        &["init", "--yes", "--install-skill", "anything"],
    )
    .await?;
    assert!(!removed.status.success());
    assert!(!home.path().join(".bitrouter/bitrouter.yaml").exists());
    isolated_config(home.path(), "127.0.0.1:0")?;
    let failed = invoke(
        home.path(),
        &[
            "init",
            "--yes",
            "--mcp-name",
            "review-peer",
            "--mcp-from",
            "missing-entry.yaml",
            "--after",
            "serve",
        ],
    )
    .await?;
    assert!(!failed.status.success());
    let report: Value = serde_json::from_slice(&failed.stdout)?;
    assert_eq!(report["resources"][0]["status"], "failed");
    assert!(home.path().join("bitrouter.yaml").exists());
    Ok(())
}
