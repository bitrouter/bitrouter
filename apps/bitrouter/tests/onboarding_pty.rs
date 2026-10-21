#![cfg(unix)]
//! Real canonical-terminal coverage, with stdout captured separately as JSON.

use std::path::Path;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize};
use serde_json::{Value, json};

#[path = "../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
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
            let mut store = bitrouter_ai::auth::file::snapshot::CredentialStore::load(
                home.path().join("data/bitrouter/oauth-tokens.json"),
            )?;
            store.set(
                "fixture-one",
                "default",
                bitrouter_ai::auth::credentials::Credential::api_key("fixture-reset-secret"),
            )?;
        }
        if configured {
            write_config(home.path())?;
        }
        let registry = bitrouter_sdk::config::RegistryConfig::default().url;
        let cache = home.path().join("cache/bitrouter");
        std::fs::create_dir_all(&cache)?;
        std::fs::write(
            cache.join("registry.json"),
            serde_json::to_vec(&json!({
                "source": registry,
                "fetched_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
                "data": { "providers": [
                    {"name": "fixture-one", "status": "active", "api_base": "http://127.0.0.1:9", "auth": {"kind":"bearer", "env":"FIXTURE_ONE_API_KEY"}},
                    {"name": "fixture-two", "status": "active", "api_base": "http://127.0.0.1:9", "auth": {"kind":"bearer", "env":"FIXTURE_TWO_API_KEY"}},
                    {"name": "claude-code", "status": "active", "api_base": "http://127.0.0.1:9", "auth": {"kind":"bearer", "env":"FIXTURE_CLAUDE_API_KEY"}}
                ], "canonical": [] }
            }))?,
        )?;
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
            std::ffi::OsStr::new(env!("CARGO_BIN_EXE_bro")),
        ]);
        command.args(args);
        command.cwd(home.path());
        command.env("HOME", home.path());
        command.env("XDG_DATA_HOME", home.path().join("data"));
        command.env("XDG_CACHE_HOME", home.path().join("cache"));
        command.env_remove("BITROUTER_HOME");
        if args.contains(&"--yes") && args.contains(&"launch") {
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
    terminal.send(b"Continue to agent setup\n")?;
    terminal.wait("Agents and resources")?;
    terminal.send(b"\n")?;
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
    terminal.send(b"Continue to agent setup\n")?;
    terminal.wait("Agents and resources")?;
    // A concurrent config creation before save is preserved, including its
    // ephemeral listen port. This keeps the real startup test isolated.
    let runtime = terminal.home.path().join(".bitrouter");
    write_config(&runtime)?;
    terminal.send(b"Skip\n")?;
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
        terminal.send(b"Continue to agent setup\n")?;
        terminal.wait("Agents and resources")?;
        terminal.send(b"Skip\n")?;
        terminal.wait("Optional resources")?;
        terminal.send(b"Continue\n")?;
        terminal.finish(true)?;
        let store = bitrouter_ai::auth::file::snapshot::CredentialStore::load(
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
fn yes_launch_with_missing_native_tool_does_not_prompt_even_on_a_terminal() -> Result<()> {
    let mut terminal = Terminal::new(
        &["init", "--yes", "--harness", "codex", "--after", "launch"],
        true,
    )?;
    let report = terminal.finish(false)?;
    assert!(report["error"].is_object());
    assert!(
        terminal.home.path().join("bitrouter.yaml").is_file(),
        "optional native launch failure must keep Router setup"
    );
    assert!(!terminal.transcript.contains("Install it now"));
    Ok(())
}

fn enter_resources(terminal: &mut Terminal) -> Result<()> {
    terminal.wait("Number/name")?;
    terminal.send(b"Continue to agent setup\n")?;
    terminal.wait("Agents and resources")?;
    terminal.send(b"Skip\n")?;
    terminal.wait("Optional resources")?;
    terminal.checkpoint();
    Ok(())
}

#[test]
fn optional_resources_retry_real_transactions_and_keep_router_progress() -> Result<()> {
    let mut terminal = Terminal::new(&["init", "--after", "exit", "--workspace", "project"], true)?;
    let source = terminal.home.path().join("review");
    std::fs::create_dir(terminal.home.path().join("project"))?;
    enter_resources(&mut terminal)?;
    assert!(terminal.home.path().join("bitrouter.yaml").is_file());
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
    terminal.send(b"Install skill\n")?;
    terminal.wait("Skill source")?;
    terminal.send(b"missing-source\n")?;
    terminal.wait("Optional skill step failed")?;
    terminal.wait("Optional resources")?;
    std::fs::create_dir(&source)?;
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: review\ndescription: PTY workflow\n---\nPTY_LOCAL_ROOT\n",
    )?;
    terminal.checkpoint();
    terminal.send(b"Install skill\n")?;
    terminal.wait("Skill source")?;
    terminal.send(format!("{}\n", source.display()).as_bytes())?;
    terminal.checkpoint();
    terminal.wait("Optional resources")?;
    terminal.send(b"Continue\n")?;
    let report = terminal.finish(true)?;
    let statuses: Vec<_> = report["resources"]
        .as_array()
        .context("resource outcomes")?
        .iter()
        .map(|row| row["status"].clone())
        .collect();
    assert_eq!(
        statuses,
        vec![
            json!("failed"),
            json!("saved"),
            json!("failed"),
            json!("installed")
        ]
    );
    assert_eq!(report["config_activation"], "saved_only");
    assert!(
        terminal
            .home
            .path()
            .join("project/.bitrouter/skills/installed.json")
            .is_file()
    );
    let config: Value = serde_saphyr::from_str(&std::fs::read_to_string(
        terminal.home.path().join("bitrouter.yaml"),
    )?)?;
    assert_eq!(
        config["mcp_servers"]["pty-peer"]["transport"]["url"],
        "http://127.0.0.1:9/mcp"
    );
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

#[test]
fn yes_on_a_terminal_rejects_ambiguous_skill_source_without_prompting() -> Result<()> {
    let mut terminal = Terminal::prepared(
        &[
            "init",
            "--yes",
            "--install-skill",
            "multi",
            "--workspace",
            "project",
            "--after",
            "exit",
        ],
        true,
        false,
        |home| {
            std::fs::create_dir(home.join("project"))?;
            for name in ["one", "two"] {
                let root = home.join("multi").join(name);
                std::fs::create_dir_all(&root)?;
                std::fs::write(
                    root.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: {name} workflow\n---\nbody\n"),
                )?;
            }
            Ok(())
        },
    )?;
    let report = terminal.finish(false)?;
    assert_eq!(report["resources"][0]["status"], "failed");
    assert!(!terminal.transcript.contains("Skill bundle"));
    assert!(!terminal.transcript.contains("Number/name"));
    assert!(terminal.home.path().join("bitrouter.yaml").is_file());
    assert!(
        !terminal
            .home
            .path()
            .join("project/.bitrouter/skills/installed.json")
            .exists()
    );
    Ok(())
}
