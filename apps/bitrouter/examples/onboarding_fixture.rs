//! Cargo-built process/terminal fixture for onboarding acceptance tests.

use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args_os()
        .next()
        .as_deref()
        .and_then(|path| std::path::Path::new(path).file_stem())
        .is_some_and(|name| name == "codex")
    {
        std::fs::write(
            std::env::var_os("ONBOARDING_NATIVE_MARKER").context("native marker")?,
            "native codex launched",
        )?;
        return Ok(());
    }
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().context("fixture mode")?;
    match mode.to_str() {
        Some("mcp-editor") => {
            let file = args.next().context("entry file")?;
            let directory = std::path::PathBuf::from(
                std::env::var_os("MCP_EDITOR_DIRECTORY").context("editor directory")?,
            );
            std::fs::write(directory.join("editor.started"), std::fs::read(&file)?)?;
            eprintln!("__MCP_EDITOR_READY__");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !directory.join("editor.release").is_file() {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "editor gate timed out"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            if std::env::var_os("MCP_EDITOR_FAIL").is_some() {
                std::process::exit(17);
            }
            std::fs::write(file, std::fs::read(directory.join("editor-input.yaml"))?)?;
            println!("__MCP_EDITOR_OUTPUT__");
        }
        Some("exit") => std::process::exit(17),
        Some("wait") => tokio::time::sleep(std::time::Duration::from_secs(60)).await,
        Some("terminal") => {
            let directory = std::path::PathBuf::from(args.next().context("fixture directory")?);
            let binary = args.next().context("bro binary")?;
            #[cfg(unix)]
            let before = rustix::termios::tcgetattr(std::io::stdin())?;
            // The child shares this foreground process group. Keep the fixture
            // alive on Ctrl-C so it can measure the child's terminal cleanup.
            #[cfg(unix)]
            let _signal =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let mut command = std::process::Command::new(binary);
            command
                .args(args)
                .stdout(std::fs::File::create(directory.join("stdout"))?);
            if std::env::var_os("ONBOARDING_REDIRECT_STDERR").is_some() {
                command.stderr(std::fs::File::create(directory.join("stderr"))?);
            }
            let mut child = command.spawn()?;
            std::fs::write(directory.join("child.pid"), child.id().to_string())?;
            let status = child.wait()?;
            #[cfg(unix)]
            let restored = format!("{before:?}")
                == format!("{:?}", rustix::termios::tcgetattr(std::io::stdin())?);
            #[cfg(not(unix))]
            let restored = true;
            std::fs::write(
                directory.join("result.json"),
                serde_json::to_vec(
                    &serde_json::json!({"success": status.success(), "restored": restored}),
                )?,
            )?;
            eprintln!("__ONBOARDING_FIXTURE_DONE__");
        }
        _ => anyhow::bail!("unknown fixture mode"),
    }
    Ok(())
}
