//! Process effects for the composer's external editor and clipboard.

use std::io::Write;

use anyhow::{Context, Result, bail, ensure};
use tokio::io::AsyncWriteExt;

/// The terminal must be suspended and its input reader stopped by the caller.
/// A failed editor leaves its file available and reports the recovery path.
pub(crate) async fn edit(draft: String) -> Result<String> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .context("Set VISUAL or EDITOR to use an external editor")?;
    let mut file = tempfile::Builder::new()
        .prefix("bitrouter-draft-")
        .suffix(".md")
        .tempfile()?;
    file.write_all(draft.as_bytes())?;
    file.flush()?;
    let path = file.into_temp_path();
    let mut command = editor_command(&editor, &path);
    let status = command.kill_on_drop(true).status().await;
    match status {
        Ok(status) if status.success() => match std::fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(error) => {
                let path = path
                    .keep()
                    .context("retaining the unreadable edited draft")?;
                bail!(
                    "Could not read edited draft: {error}; original draft retained in the composer; edited file: {}",
                    path.display()
                )
            }
        },
        result => {
            let path = path
                .keep()
                .context("retaining the recoverable editor draft")?;
            bail!(
                "Editor failed ({result:?}); original draft retained in the composer; edited file: {}",
                path.display()
            )
        }
    }
}

fn editor_command(editor: &str, path: &std::path::Path) -> tokio::process::Command {
    #[cfg(unix)]
    {
        // The editor is an explicitly configured shell command. The generated
        // filename remains a separate positional argument, never shell code.
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!("exec {editor} \"$1\""))
            .arg("bitrouter-editor")
            .arg(path);
        command
    }
    #[cfg(not(unix))]
    {
        let mut command = tokio::process::Command::new("cmd");
        command.arg("/C").arg(editor).arg(path);
        command
    }
}

pub(crate) async fn copy(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(target_os = "windows")]
    let candidates: &[(&str, &[&str])] = &[("clip", &[])];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: &[(&str, &[&str])] =
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];
    for (program, args) in candidates {
        let child = tokio::process::Command::new(program)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = child else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await?;
        }
        ensure!(
            child.wait().await?.success(),
            "Clipboard command failed; use the output inspector to select and copy the text"
        );
        return Ok(());
    }
    bail!("Clipboard unavailable; use the output inspector to select and copy the text")
}

#[cfg(all(test, unix))]
mod tests {
    #[tokio::test]
    async fn editor_path_is_an_argument_and_prompt_bytes_remain_intact() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("draft ' $(touch unintended).md");
        let original = "  中文 👩‍💻\n\nkeep trailing spaces  \n";
        std::fs::write(&path, original)?;
        let result = super::editor_command("test -f", &path).status().await?;
        assert!(result.success());
        assert_eq!(std::fs::read_to_string(path)?, original);
        assert!(!dir.path().join("unintended").exists());
        Ok(())
    }
}
