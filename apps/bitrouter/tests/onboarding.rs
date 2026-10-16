//! Binary-level coverage for the `bitrouter` onboarding front door — bare
//! invocation (wizard-vs-status decision), the `init --yes` headless contract,
//! and the network-free / `BITROUTER_HOME`-tolerant probe. Every case here is
//! hermetic: an isolated `HOME` + `XDG_DATA_HOME`, all BYOK env vars removed,
//! stdin nulled (non-TTY) except explicit PTY cases, and no network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

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

fn seed_onboarding_state(home: &Path, data_home: &Path) -> anyhow::Result<Vec<(PathBuf, Vec<u8>)>> {
    use bitrouter::gateway::providers::auth::credentials::Credential;
    use bitrouter::gateway::providers::auth::file::snapshot::CredentialStore;

    let config = home.join("bitrouter.yaml");
    let cloud = data_home.join("bitrouter/account-credentials.json");
    let providers = data_home.join("bitrouter/oauth-tokens.json");
    std::fs::write(
        &config,
        "server: { listen: '127.0.0.1:9012', skip_auth: false }\n",
    )?;
    std::fs::create_dir_all(data_home.join("bitrouter"))?;
    std::fs::write(
        &cloud,
        r#"{"kind":"api_key","api_key":"brk_existing.fixture","base_url":"https://api.bitrouter.ai"}"#,
    )?;
    CredentialStore::load(&providers)?.set(
        "openai",
        "default",
        Credential::api_key("sk-existing"),
    )?;
    [config, cloud, providers]
        .into_iter()
        .map(|path| Ok((path.clone(), std::fs::read(path)?)))
        .collect()
}

#[cfg(not(feature = "tui"))]
#[test]
fn unavailable_launch_rejects_before_reset_credentials_config_or_logs() -> anyhow::Result<()> {
    for yes in [false, true] {
        for existing in [false, true] {
            let home = TempDir::new()?;
            let data = TempDir::new()?;
            let saved = if existing {
                seed_onboarding_state(home.path(), data.path())?
            } else {
                Vec::new()
            };
            let config = home.path().join("bitrouter.yaml");
            let mut args = vec![
                "init",
                "--reset",
                "--force",
                "--after",
                "launch",
                "--api-key",
                "brk_replacement.fixture",
                "--provider",
                "openai",
                "--provider-api-key",
                "sk-replacement",
                "-c",
                config
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-UTF-8 test path"))?,
            ];
            if yes {
                args.push("--yes");
            }
            let out = run_cli(home.path(), data.path(), &args, &[]);
            let diagnostic = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(!out.status.success(), "{diagnostic}");
            assert!(
                diagnostic.contains("--after launch requires the tui feature"),
                "{diagnostic}"
            );
            for (path, bytes) in saved {
                assert_eq!(
                    std::fs::read(&path)?,
                    bytes,
                    "{} was changed",
                    path.display()
                );
            }
            if !existing {
                assert!(!config.exists());
                assert!(!data.path().join("bitrouter").exists());
            }
            assert!(
                !home.path().join(".bitrouter").exists(),
                "preflight created runtime state"
            );
        }
    }
    Ok(())
}

#[test]
fn non_tty_reset_without_yes_preserves_provider_credentials() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    let saved = seed_onboarding_state(home.path(), data.path())?;
    let provider_path = data.path().join("bitrouter/oauth-tokens.json");
    let out = run_cli(
        home.path(),
        data.path(),
        &["init", "--reset", "--after", "exit"],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout_json(&out)["after"], "exit");
    assert!(
        !data
            .path()
            .join("bitrouter/account-credentials.json")
            .exists()
    );
    for (path, bytes) in saved {
        if path == provider_path {
            assert_eq!(
                std::fs::read(path)?,
                bytes,
                "non-TTY reset granted provider removal consent"
            );
        }
    }
    Ok(())
}

#[cfg(all(unix, not(feature = "tui")))]
fn run_cli_pty(
    home: &Path,
    data_home: &Path,
    args: &[&str],
) -> anyhow::Result<(portable_pty::ExitStatus, String)> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;
    use std::time::{Duration, Instant};

    let pty = native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let reader = pty.master.try_clone_reader()?;
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_bro"));
    command.args(args);
    command.env_clear();
    command.env("HOME", home);
    command.env("XDG_DATA_HOME", data_home);
    command.env("TERM", "xterm-256color");
    command.cwd(home);
    let mut child = pty.slave.spawn_command(command)?;
    drop(pty.slave);
    let result = (|| -> anyhow::Result<_> {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("onboarding-pty".into())
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
            anyhow::ensure!(Instant::now() < deadline, "unavailable UI did not exit");
            std::thread::sleep(Duration::from_millis(10));
        };
        let (read, output) = receiver.recv_timeout(Duration::from_secs(5))?;
        read?;
        Ok((status, String::from_utf8(output)?))
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(all(unix, not(feature = "tui")))]
#[test]
fn unavailable_wizard_rejects_before_reset_or_flag_writes() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    let saved = seed_onboarding_state(home.path(), data.path())?;
    let (status, output) = run_cli_pty(
        home.path(),
        data.path(),
        &[
            "init",
            "--reset",
            "--force",
            "--after",
            "exit",
            "--api-key",
            "brk_replacement.fixture",
            "--provider",
            "openai",
            "--provider-api-key",
            "sk-replacement",
        ],
    )?;
    assert!(!status.success(), "{output}");
    assert!(
        output.contains("interactive onboarding requires the tui feature"),
        "{output}"
    );
    assert!(
        !output.contains("\u{1b}[?1049h"),
        "terminal drawing started"
    );
    for (path, bytes) in saved {
        assert_eq!(
            std::fs::read(&path)?,
            bytes,
            "{} was changed",
            path.display()
        );
    }
    assert!(
        !home.path().join(".bitrouter").exists(),
        "preflight created runtime state"
    );
    Ok(())
}

#[cfg(all(unix, not(feature = "tui")))]
#[test]
fn bare_interactive_entry_requires_tui_without_saving_or_launching() -> anyhow::Result<()> {
    for configured in [false, true] {
        let home = TempDir::new()?;
        let data = TempDir::new()?;
        let config = home.path().join("bitrouter.yaml");
        let original = "inherit_defaults: false\nchat: { agent: codex-acp }\n";
        if configured {
            std::fs::write(&config, original)?;
        }
        let (status, output) = run_cli_pty(home.path(), data.path(), &[])?;
        assert!(!status.success(), "{output}");
        let expected = if configured {
            "interactive default chat requires the tui feature"
        } else {
            "interactive onboarding requires the tui feature"
        };
        assert!(output.contains(expected), "{output}");
        assert!(
            !output.contains("\u{1b}[?1049h"),
            "terminal drawing started"
        );
        if configured {
            assert_eq!(std::fs::read_to_string(config)?, original);
        } else {
            assert!(!config.exists());
        }
        assert!(!home.path().join(".bitrouter/bitrouter.yaml").exists());
        assert!(!home.path().join("bitrouter.db").exists());
        assert!(!data.path().join("bitrouter").exists());
    }
    Ok(())
}

#[cfg(not(feature = "harness"))]
#[test]
fn bare_entry_without_harness_never_creates_session_logs() -> anyhow::Result<()> {
    for saved in [
        None,
        Some("inherit_defaults: false\n"),
        Some("inherit_defaults: false\nchat: { agent: codex-acp }\n"),
    ] {
        let home = TempDir::new()?;
        let data = TempDir::new()?;
        let config = home.path().join("bitrouter.yaml");
        if let Some(raw) = saved {
            std::fs::write(&config, raw)?;
        }
        let output = run_cli(home.path(), data.path(), &[], &[]);
        if saved.is_some_and(|raw| raw.contains("codex-acp")) {
            assert!(!output.status.success());
            let diagnostic = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(diagnostic.contains("harness feature"), "{diagnostic}");
        } else {
            assert!(output.status.success());
        }
        assert!(!home.path().join(".bitrouter").exists());
        assert!(!home.path().join("logs").exists());
        assert!(std::fs::read_dir(data.path())?.next().is_none());
        if let Some(raw) = saved {
            assert_eq!(std::fs::read_to_string(config)?, raw);
        }
    }
    Ok(())
}

#[test]
fn bare_unconfigured_emits_inert_envelope_and_exits_zero() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    // Nothing configured + no TTY: the wizard can't run, so onboarding prints
    // the hint to stderr and emits an inert envelope — never hangs, exit 0.
    let out = run_cli(home.path(), data.path(), &[], &[]);
    assert!(out.status.success(), "bare bitrouter must exit 0");
    let v = stdout_json(&out);
    assert_eq!(v["action"], "onboarding");
    assert_eq!(v["providers_configured"], serde_json::json!([]));
    assert_eq!(v["after"], "exit");
    assert!(v.get("config_activation").is_none());
}

#[test]
fn credentials_alone_do_not_complete_onboarding() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    // Provider credentials do not substitute for a saved default ACP harness.
    let out = run_cli(
        home.path(),
        data.path(),
        &[],
        &[("OPENAI_API_KEY", "sk-detected")],
    );
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["action"], "onboarding");
    // Non-interactive bare invocation does not silently save onboarding.
    assert!(!home.path().join("bitrouter.yaml").exists());
}

#[test]
fn bitrouter_home_set_but_missing_offers_onboarding_not_a_hard_error() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    // BITROUTER_HOME points at a directory with no bitrouter.yaml — which
    // `resolve_config` treats as a hard error. The probe-based onboarding entry
    // must sidestep that: exit 0, emit the envelope, and never surface the
    // "BITROUTER_HOME is set … but … is missing" error.
    let br_home = TempDir::new().unwrap();
    let out = run_cli(
        home.path(),
        data.path(),
        &[],
        &[("BITROUTER_HOME", br_home.path().to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "BITROUTER_HOME-missing must not hard-error bare bitrouter"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("BITROUTER_HOME is set"),
        "must not surface the resolve_config hard error: {stderr}"
    );
    let v = stdout_json(&out);
    assert_eq!(v["action"], "onboarding");
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
    assert_eq!(v["harnesses_installed"], serde_json::json!(["codex-acp"]));
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

#[cfg(all(unix, feature = "tui"))]
#[tokio::test]
async fn onboarding_launch_and_next_bare_invocation_both_speak_acp() -> anyhow::Result<()> {
    onboarding_then_bare_invocation("launch").await
}

#[cfg(all(unix, feature = "harness"))]
#[tokio::test]
async fn onboarding_exit_and_next_bare_invocation_preserve_piped_chat() -> anyhow::Result<()> {
    onboarding_then_bare_invocation("exit").await
}

#[cfg(all(unix, feature = "harness"))]
async fn onboarding_then_bare_invocation(after: &str) -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    let config_path = home.path().join("bitrouter.yaml");
    let marker = home.path().join("acp-wire");
    let script = r#"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$ACP_MARKER"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *initialize*) printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
    *session/new*) printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"onboarding-acp"}}\n' "$id";;
  esac
done
"#;
    std::fs::write(
        &config_path,
        serde_json::to_string(&serde_json::json!({
            "inherit_defaults": false,
            "agents": { "codex-acp": { "name": "codex-acp", "transport": {
                "type": "stdio", "command": "/bin/sh", "args": ["-c", script],
                "env": {"ACP_MARKER": marker}
            } } }
        }))?,
    )?;
    for args in [
        vec!["init", "--yes", "--harness", "codex", "--after", after],
        vec![],
    ] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .args(&args)
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("XDG_DATA_HOME", data.path())
            .env_remove("BITROUTER_HOME")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        for var in PROBE_VARS {
            command.env_remove(var);
        }
        let out =
            tokio::time::timeout(std::time::Duration::from_secs(15), command.output()).await??;
        assert!(
            out.status.success(),
            "args={args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        if !args.is_empty() && after == "exit" {
            assert!(!marker.exists(), "setup with --after exit launched ACP");
            continue;
        }
        let wire = std::fs::read_to_string(&marker)?;
        assert!(wire.contains("initialize"), "{wire}");
        assert!(wire.contains("session/new"), "{wire}");
        std::fs::remove_file(&marker)?;
        assert!(!String::from_utf8_lossy(&out.stdout).contains("\"action\":\"status\""));
    }
    Ok(())
}
