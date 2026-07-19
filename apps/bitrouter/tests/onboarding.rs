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
fn bare_configured_prints_status_not_wizard() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    // A detected BYOK env key makes the probe report "configured": bare
    // bitrouter prints a status view (action=status), never the wizard.
    let out = run_cli(
        home.path(),
        data.path(),
        &[],
        &[("OPENAI_API_KEY", "sk-detected")],
    );
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["action"], "status");
    assert_eq!(v["configured"], true);
    // No config file is written as a side effect of a status view.
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
    assert_eq!(v["harnesses_installed"], serde_json::json!([]));
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
fn init_yes_refuses_overwrite_without_force() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let cfg = home.path().join("bitrouter.yaml");
    std::fs::write(&cfg, "# hand-tuned\n").unwrap();

    // Without --force, an existing config is left untouched.
    let out = run_cli(
        home.path(),
        data.path(),
        &["init", "--yes", "-c", cfg.to_str().unwrap()],
        &[],
    );
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "# hand-tuned\n");

    // With --force it overwrites with the starter template.
    let out = run_cli(
        home.path(),
        data.path(),
        &["init", "--yes", "--force", "-c", cfg.to_str().unwrap()],
        &[],
    );
    assert!(out.status.success());
    assert!(
        std::fs::read_to_string(&cfg)
            .unwrap()
            .contains("skip_auth: true")
    );
}
