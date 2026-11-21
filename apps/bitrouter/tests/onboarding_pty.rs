#![cfg(unix)]
//! Real canonical-terminal coverage, with stdout captured separately as JSON.

use std::path::Path;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[path = "support/example_binary.rs"]
mod example_binary;

#[path = "support/terminal.rs"]
mod terminal;
use terminal::Terminal;

impl Terminal {
    fn new(args: &[&str], configured: bool) -> Result<Self> {
        Self::with_stderr(args, configured, false)
    }

    fn with_stderr(args: &[&str], configured: bool, redirected: bool) -> Result<Self> {
        Self::prepared(args, configured, redirected, |_| Ok(()))
    }

    fn prepared(
        args: &[&str],
        configured: bool,
        redirected: bool,
        prepare: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<Self> {
        static FIXTURE: std::sync::OnceLock<Result<std::path::PathBuf, String>> =
            std::sync::OnceLock::new();
        let fixture = FIXTURE
            .get_or_init(|| example_binary::build("bitrouter", "onboarding_fixture"))
            .clone()
            .map_err(anyhow::Error::msg)?;
        let home = tempfile::tempdir_in("/tmp")?;
        prepare(home.path())?;
        if args.contains(&"--reset") {
            let mut store =
                bitrouter::gateway::providers::auth::file::snapshot::CredentialStore::load(
                    home.path().join("data/bitrouter/oauth-tokens.json"),
                )?;
            store.set(
                "fixture-one",
                "default",
                bitrouter::gateway::providers::auth::credentials::Credential::api_key(
                    "fixture-reset-secret",
                ),
            )?;
        }
        if configured {
            write_config(home.path())?;
        }
        let registry = bitrouter::config::RegistryConfig::default()
            .source()
            .to_owned();
        let cache = home.path().join("cache/bitrouter/registry-v1");
        std::fs::create_dir_all(&cache)?;
        let document = serde_json::to_vec(&json!({
            "version": 1,
            "spec_revision": bitrouter_registry::document::SPEC_REVISION,
            "as_of": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "data": { "models": [], "providers": [
                {"name": "fixture-one", "status": "active", "billing": "usage", "models": [], "api_base": "https://fixture.invalid/v1", "auth": {"kind":"bearer", "env":"FIXTURE_ONE_API_KEY"}},
                {"name": "fixture-two", "status": "active", "billing": "usage", "models": [], "api_base": "https://fixture.invalid/v1", "auth": {"kind":"bearer", "env":"FIXTURE_TWO_API_KEY"}},
                {"name": "claude-code", "status": "active", "billing": "usage", "models": [], "api_base": "https://fixture.invalid/v1", "auth": {"kind":"bearer", "env":"FIXTURE_CLAUDE_API_KEY"}}
            ] }
        }))?;
        bitrouter_registry::document::Document::parse(
            &document,
            &bitrouter_registry::document::ReaderPolicy::default(),
        )?;
        let mut cached = serde_json::to_vec(&json!({
            "cache_version": 1, "source": registry,
            "checked_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()
        }))?;
        cached.push(b'\n');
        cached.extend(document);
        let key = hex::encode(Sha256::digest(format!("1\0{registry}").as_bytes()));
        std::fs::write(cache.join(format!("{key}.json")), cached)?;
        // Init sees our cached provider choices; the serving runtime stays
        // offline because inherit_defaults=false gates the registry merge.
        if configured {
            let path = home.path().join("bitrouter.yaml");
            let config = std::fs::read_to_string(&path)?.replace("enabled: false", "enabled: true");
            std::fs::write(path, config)?;
        }
        let mut command = CommandBuilder::new(fixture);
        command.args([
            std::ffi::OsStr::new("terminal"),
            home.path().as_os_str(),
            std::ffi::OsStr::new(env!("CARGO_BIN_EXE_bitrouter")),
        ]);
        command.args(args);
        command.cwd(home.path());
        command.env("HOME", home.path());
        command.env("XDG_DATA_HOME", home.path().join("data"));
        command.env("XDG_CACHE_HOME", home.path().join("cache"));
        command.env_remove("BITROUTER_HOME");
        if args.contains(&"--yes") {
            command.env("PATH", home.path());
        }
        command.env_remove("ONBOARDING_REDIRECT_STDERR");
        if redirected {
            command.env("ONBOARDING_REDIRECT_STDERR", "1");
        }
        for name in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENCODE_ZEN_API_KEY",
            "BITROUTER_API_KEY",
        ] {
            command.env_remove(name);
        }
        Self::spawn(
            home,
            command,
            PtySize {
                rows: 40,
                cols: 160,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
    }

    fn checkpoint(&mut self) {
        self.transcript.clear();
    }
}

fn write_config(home: &Path) -> Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    std::fs::create_dir_all(home)?;
    std::fs::write(
        home.join("bitrouter.yaml"),
        format!(
            "inherit_defaults: false\nregistry: {{ enabled: false }}\nserver: {{ listen: '{address}', skip_auth: true }}\ndatabase: {{ url: 'sqlite::memory:' }}\n"
        ),
    )?;
    Ok(())
}

#[test]
fn provider_retry_repeated_addition_search_skip_and_json_are_canonical_lines() -> Result<()> {
    let mut terminal = Terminal::new(&["init", "--after", "exit"], true)?;
    terminal.wait("Number/name")?;
    terminal.send(b"999999\n")?;
    terminal.wait("Choose a number from 1")?;
    terminal.checkpoint();
    terminal.send(b"fixture-\n")?;
    terminal.wait("fixture-one")?;
    terminal.send(b"fixture-one\n")?;
    terminal.wait("input hidden")?;
    terminal.checkpoint();
    terminal.send(b"\n")?;
    terminal.wait("no API key entered")?;
    terminal.checkpoint();
    terminal.send(b"fixture-one\n")?;
    terminal.wait("input hidden")?;
    terminal.checkpoint();
    terminal.send(b"fixture-secret-one\n")?;
    terminal.wait("Add another provider")?;
    assert!(!terminal.transcript.contains("fixture-secret-one"));
    terminal.checkpoint();
    terminal.send(b"fixture-two\n")?;
    terminal.wait("input hidden")?;
    terminal.checkpoint();
    terminal.send(b"fixture-secret-two\n")?;
    terminal.wait("Add another provider")?;
    assert!(!terminal.transcript.contains("fixture-secret-two"));
    terminal.checkpoint();
    terminal.send(b"Continue to optional resources\n")?;
    terminal.wait("Optional resources")?;
    terminal.send(b"Continue\n")?;
    let report = terminal.finish(true)?;
    assert_eq!(
        report["providers_configured"],
        json!(["fixture-one", "fixture-two"])
    );
    assert_eq!(report["agents_selected"], json!([]));
    assert_eq!(report["harnesses_installed"], json!([]));
    assert!(
        report["providers_skipped_interactive"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert!(!report.to_string().contains("fixture-secret"));
    let saved: Value = serde_saphyr::from_str(&std::fs::read_to_string(
        terminal.home.path().join("bitrouter.yaml"),
    )?)?;
    assert!(saved.get("chat").is_none());
    Ok(())
}

#[test]
fn eof_and_ctrl_c_restore_secret_echo_and_do_not_save_configuration() -> Result<()> {
    for cancel in *b"\x03\x04" {
        let mut terminal = Terminal::new(&["init", "--after", "exit"], true)?;
        let path = terminal.home.path().join("bitrouter.yaml");
        let before = std::fs::read(&path)?;
        terminal.wait("Number/name")?;
        terminal.send(b"fixture-one\n")?;
        terminal.wait("input hidden")?;
        terminal.send(&[cancel])?;
        let report = terminal.finish(false)?;
        assert!(report["error"].is_object());
        assert_eq!(std::fs::read(path)?, before);
    }
    Ok(())
}

#[test]
fn first_bare_onboarding_saves_then_starts_background_daemon_and_returns() -> Result<()> {
    let mut terminal = Terminal::new(&[], false)?;
    terminal.wait("Number/name")?;
    // Publish a config while the provider prompt is still open, before the
    // gateway saves it. Its ephemeral port keeps the real startup isolated.
    let runtime = terminal.home.path().join(".bitrouter");
    write_config(&runtime)?;
    terminal.send(b"Continue to optional resources\n")?;
    terminal.wait("Optional resources")?;
    terminal.send(b"Continue\n")?;
    let report = terminal.finish(true)?;
    let pid = report["daemon"]["pid"]
        .as_u64()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
        .context("daemon readiness report missing")?;
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    assert!(terminal.transcript.contains("Step 3/3 — Finish (serve)"));
    assert_eq!(report["after"], "serve");
    assert_eq!(report["daemon"]["status"], "ready");
    assert_eq!(report["config_activation"], "saved_only");
    assert!(runtime.join("bitrouter.yaml").is_file());
    Ok(())
}

#[test]
fn login_method_selection_uses_submitted_names_and_hides_input() -> Result<()> {
    let mut terminal = Terminal::new(&["init", "--after", "exit"], true)?;
    terminal.wait("Number/name")?;
    terminal.send(b"claude-code\n")?;
    terminal.wait("Choose an authentication method")?;
    terminal.send(b"API key (paste a static credential)\n")?;
    terminal.wait("input hidden")?;
    terminal.send(b"\x04")?;
    assert!(terminal.finish(false)?["error"].is_object());
    Ok(())
}

#[test]
fn reset_uses_yes_no_confirmation_and_default_no_keeps_credentials() -> Result<()> {
    for answer in [b"\n".as_slice(), b"yes\n".as_slice()] {
        let mut terminal = Terminal::new(&["init", "--reset", "--after", "exit"], true)?;
        terminal.wait("[y/N]")?;
        terminal.send(b"invalid\n")?;
        terminal.wait("Enter yes or no")?;
        terminal.send(answer)?;
        terminal.wait("Number/name")?;
        terminal.send(b"Continue to optional resources\n")?;
        terminal.wait("Optional resources")?;
        terminal.send(b"Continue\n")?;
        terminal.finish(true)?;
        let store = bitrouter::gateway::providers::auth::file::snapshot::CredentialStore::load(
            terminal
                .home
                .path()
                .join("data/bitrouter/oauth-tokens.json"),
        )?;
        assert_eq!(store.providers().is_empty(), answer == b"yes\n");
    }
    Ok(())
}

#[test]
fn unconfigured_bare_cancels_or_rejects_redirected_stderr_without_saving() -> Result<()> {
    for input in *b"\x03\x04" {
        let mut terminal = Terminal::new(&[], false)?;
        terminal.wait("Number/name")?;
        terminal.send(&[input])?;
        assert!(terminal.finish(false)?["error"].is_object());
        assert!(
            !terminal
                .home
                .path()
                .join(".bitrouter/bitrouter.yaml")
                .exists()
        );
    }
    let mut terminal = Terminal::with_stderr(&[], false, true)?;
    assert!(
        terminal.finish(false)?["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("stdin and stderr terminals"))
    );
    assert!(
        !terminal
            .home
            .path()
            .join(".bitrouter/bitrouter.yaml")
            .exists()
    );
    Ok(())
}

#[test]
fn headless_gateway_setup_needs_no_native_tool_and_refuses_agent_launch_flags() -> Result<()> {
    // Both children have an empty executable search path but a real terminal.
    let mut setup = Terminal::new(&["init", "--yes", "--after", "exit"], false)?;
    let report = setup.finish(true)?;
    assert_eq!(report["after"], "exit");
    assert!(
        setup
            .home
            .path()
            .join(".bitrouter/bitrouter.yaml")
            .is_file()
    );
    assert!(!setup.transcript.contains("Install it now"));

    let mut obsolete = Terminal::new(
        &["init", "--yes", "--harness", "codex", "--after", "launch"],
        true,
    )?;
    let config = obsolete.home.path().join("bitrouter.yaml");
    let before = std::fs::read(&config)?;
    assert!(obsolete.finish_stdout(false)?.is_empty());
    assert!(
        obsolete.transcript.lines().any(|line| {
            // Clap colors the option name independently on a color-capable TTY.
            line.contains("unexpected argument")
                && line.contains("--harness")
                && line.contains("found")
        }),
        "{}",
        obsolete.transcript
    );
    assert!(!obsolete.transcript.contains("Install it now"));
    assert_eq!(std::fs::read(config)?, before);
    Ok(())
}

fn enter_resources(terminal: &mut Terminal) -> Result<()> {
    terminal.wait("Number/name")?;
    terminal.send(b"Continue to optional resources\n")?;
    terminal.wait("Optional resources")?;
    terminal.checkpoint();
    Ok(())
}

#[test]
fn optional_resources_retry_real_transactions_and_keep_router_progress() -> Result<()> {
    let mut terminal = Terminal::new(&["init", "--after", "exit"], true)?;
    enter_resources(&mut terminal)?;
    let path = terminal.home.path().join("bitrouter.yaml");
    let saved = std::fs::read(&path)?;
    terminal.send(b"Add MCP server\n")?;
    terminal.wait("MCP server name")?;
    terminal.send(b"pty-peer\n")?;
    terminal.wait("MCP transport")?;
    terminal.send(b"http\n")?;
    terminal.wait("HTTP URL")?;
    terminal.checkpoint();
    terminal.send(b"invalid-url\n")?;
    terminal.wait("Optional mcp step failed")?;
    terminal.wait("Optional resources")?;
    terminal.checkpoint();
    terminal.send(b"Add MCP server\n")?;
    terminal.wait("MCP server name")?;
    terminal.send(b"pty-peer\n")?;
    terminal.wait("MCP transport")?;
    terminal.send(b"http\n")?;
    terminal.wait("HTTP URL")?;
    terminal.checkpoint();
    terminal.send(b"http://127.0.0.1:9/mcp\n")?;
    terminal.wait("Optional resources")?;
    terminal.checkpoint();
    terminal.send(b"Continue\n")?;
    let report = terminal.finish(true)?;
    let statuses: Vec<_> = report["resources"]
        .as_array()
        .context("resource outcomes")?
        .iter()
        .map(|row| row["status"].clone())
        .collect();
    assert_eq!(report["config_activation"], "saved_only");
    if cfg!(feature = "mcp") {
        assert_eq!(statuses, vec![json!("failed"), json!("saved")]);
        let config: Value = serde_saphyr::from_str(&std::fs::read_to_string(path)?)?;
        assert_eq!(
            config["mcp_servers"]["pty-peer"]["transport"]["url"],
            "http://127.0.0.1:9/mcp"
        );
    } else {
        assert_eq!(statuses, vec![json!("failed"), json!("failed")]);
        assert!(
            report["resources"][1]["failure"]
                .as_str()
                .is_some_and(|error| error.contains("host configuration"))
        );
        assert_eq!(std::fs::read(path)?, saved);
    }
    Ok(())
}

#[test]
fn optional_resource_input_cancellation_preserves_router_and_stops_following_actions() -> Result<()>
{
    for cancel in *b"\x03\x04" {
        let mut terminal = Terminal::new(&["init", "--after", "serve"], true)?;
        enter_resources(&mut terminal)?;
        let config = terminal.home.path().join("bitrouter.yaml");
        let before = std::fs::read(&config)?;
        terminal.send(b"Add MCP server\n")?;
        terminal.wait("MCP server name")?;
        terminal.send(&[cancel])?;
        assert!(terminal.finish(false)?["error"].is_object());
        assert_eq!(std::fs::read(config)?, before);
        assert!(
            !terminal
                .home
                .path()
                .join(".bitrouter/bitrouter.sock")
                .exists()
        );
    }
    Ok(())
}
