//! Binary-level coverage for the `bitrouter` onboarding front door — bare
//! invocation (wizard-vs-status decision), the `init --yes` headless contract,
//! and the network-free / `BITROUTER_HOME`-tolerant probe. Every case here is
//! hermetic: an isolated `HOME` + `XDG_DATA_HOME`, all BYOK env vars removed,
//! stdin nulled (non-TTY), and no network.

use std::path::Path;
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_bitrouter"));
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
fn native_interactive_entry_points_are_rejected() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let data = TempDir::new()?;
    for args in [
        vec!["launch", "--agent", "codex"],
        vec!["spawn", "--agent", "codex"],
    ] {
        let out = run_cli(home.path(), data.path(), &args, &[]);
        assert!(!out.status.success());
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn onboarding_launch_and_next_bare_invocation_both_speak_acp() -> anyhow::Result<()> {
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
        vec!["init", "--yes", "--harness", "codex", "--after", "launch"],
        vec![],
    ] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"));
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
        let wire = std::fs::read_to_string(&marker)?;
        assert!(wire.contains("initialize"), "{wire}");
        assert!(wire.contains("session/new"), "{wire}");
        std::fs::remove_file(&marker)?;
        assert!(!String::from_utf8_lossy(&out.stdout).contains("\"action\":\"status\""));
    }
    Ok(())
}
