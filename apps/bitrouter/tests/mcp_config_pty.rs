#![cfg(unix)]
//! Canonical-terminal MCP editing and skill selection; stdout remains JSON.

use anyhow::{Context, Result, ensure};
use portable_pty::{CommandBuilder, PtySize};

#[path = "../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
mod example_binary;

#[path = "support/terminal.rs"]
mod terminal;
use terminal::Terminal;

impl Terminal {
    fn start(args: &[&str], editor_fail: bool) -> Result<Self> {
        Self::start_for("mcp", args, editor_fail)
    }
    fn start_for(namespace: &str, args: &[&str], editor_fail: bool) -> Result<Self> {
        static FIXTURE: std::sync::OnceLock<Result<std::path::PathBuf, String>> =
            std::sync::OnceLock::new();
        let fixture = FIXTURE
            .get_or_init(|| example_binary::build("bitrouter", "onboarding_fixture"))
            .clone()
            .map_err(anyhow::Error::msg)?;
        let home = tempfile::tempdir_in("/tmp")?;
        std::fs::write(
            home.path().join("bitrouter.yaml"),
            "inherit_defaults: false\n# keep\nmcp_servers:\n  one:\n    name: one\n    transport: {type: stdio, command: existing}\n",
        )?;
        if namespace == "skills" {
            for name in ["alpha", "beta"] {
                let source = home.path().join("source").join(name);
                std::fs::create_dir_all(&source)?;
                std::fs::write(
                    source.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: terminal fixture\n---\n"),
                )?;
            }
        }
        let mut command = CommandBuilder::new(&fixture);
        command.args([
            std::ffi::OsStr::new("terminal"),
            home.path().as_os_str(),
            std::ffi::OsStr::new(env!("CARGO_BIN_EXE_bro")),
        ]);
        command.args([namespace]);
        command.args(args);
        command.cwd(home.path());
        command.env("HOME", home.path());
        command.env("BITROUTER_HOME", home.path());
        command.env("CODEX_HOME", home.path().join("codex"));
        command.env("XDG_DATA_HOME", home.path().join("data"));
        command.env("XDG_CACHE_HOME", home.path().join("cache"));
        command.env("MCP_EDITOR_DIRECTORY", home.path());
        command.env(
            "EDITOR",
            format!(
                "{} mcp-editor",
                shell_words::quote(fixture.to_str().context("fixture path")?)
            ),
        );
        command.env_remove("MCP_EDITOR_FAIL");
        if editor_fail {
            command.env("MCP_EDITOR_FAIL", "1");
        }
        Self::spawn(
            home,
            command,
            PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
    }

    fn release(&self, payload: &str) -> Result<()> {
        std::fs::write(self.home.path().join("editor-input.yaml"), payload)?;
        std::fs::write(self.home.path().join("editor.release"), "")?;
        Ok(())
    }
}

#[test]
fn guided_add_and_eof_leave_canonical_terminal_and_json_output() -> Result<()> {
    let mut terminal = Terminal::start(&["add", "two"], false)?;
    terminal.wait("MCP transport")?;
    terminal.send(b"1\n")?;
    terminal.wait("HTTP URL:")?;
    terminal.send(b"https://example.com/mcp\n")?;
    ensure!(terminal.finish(true)?["activation"] == "saved_only");
    let raw = std::fs::read_to_string(terminal.home.path().join("bitrouter.yaml"))?;
    ensure!(raw.contains("# keep") && raw.contains("https://example.com/mcp"));
    for cancel in [b"\x04", b"\x03"] {
        let mut cancelled = Terminal::start(&["add", "two"], false)?;
        let path = cancelled.home.path().join("bitrouter.yaml");
        let original = std::fs::read(&path)?;
        cancelled.wait("MCP transport")?;
        cancelled.send(b"1\n")?;
        cancelled.wait("HTTP URL:")?;
        cancelled.send(cancel)?;
        cancelled.finish(false)?;
        ensure!(std::fs::read(path)? == original);
    }
    Ok(())
}

#[test]
fn editor_receives_one_raw_entry_and_output_does_not_corrupt_json() -> Result<()> {
    let mut terminal = Terminal::start(&["edit", "one"], false)?;
    terminal.wait("__MCP_EDITOR_READY__")?;
    let input = std::fs::read_to_string(terminal.home.path().join("editor.started"))?;
    ensure!(
        input.contains("existing")
            && !input.contains("mcp_servers")
            && !input.contains("inherit_defaults")
    );
    terminal.release("name: one\ntransport: {type: stdio, command: changed}\n")?;
    ensure!(terminal.finish(true)?["activation"] == "saved_only");
    ensure!(terminal.transcript.contains("__MCP_EDITOR_OUTPUT__"));
    ensure!(
        std::fs::read_to_string(terminal.home.path().join("bitrouter.yaml"))?.contains("changed")
    );
    Ok(())
}

#[test]
fn editor_detects_concurrent_changes_and_does_not_save_failed_edits() -> Result<()> {
    for fail in [false, true] {
        let mut terminal = Terminal::start(&["edit", "one"], fail)?;
        terminal.wait("__MCP_EDITOR_READY__")?;
        let path = terminal.home.path().join("bitrouter.yaml");
        let original = std::fs::read_to_string(&path)?;
        let expected = if fail {
            original
        } else {
            format!("# concurrent user edit\n{original}")
        };
        std::fs::write(&path, &expected)?;
        terminal.release("name: one\ntransport: {type: stdio, command: must-not-save}\n")?;
        let error = terminal.finish(false)?;
        ensure!(error["error"].is_object());
        ensure!(std::fs::read_to_string(path)? == expected);
    }
    let mut cancelled = Terminal::start(&["edit", "one"], false)?;
    cancelled.wait("__MCP_EDITOR_READY__")?;
    let path = cancelled.home.path().join("bitrouter.yaml");
    let original = std::fs::read(&path)?;
    cancelled.send(b"\x03")?;
    cancelled.finish(false)?;
    ensure!(std::fs::read(path)? == original);
    Ok(())
}
