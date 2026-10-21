//! Real CLI directory operations; acquisition is owned by external installers.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::Path;
use tokio::process::Command;
fn body(name: &str, marker: &str) -> String {
    format!("---\nname: {name}\ndescription: directory skill\n---\n{marker}\n")
}
fn source(root: &Path, name: &str) -> Result<()> {
    let skill = root.join(name);
    std::fs::create_dir_all(skill.join("references"))?;
    std::fs::write(skill.join("SKILL.md"), body(name, "ORIGINAL"))?;
    std::fs::write(skill.join("references/check.md"), "support")?;
    Ok(())
}
async fn run(root: &Path, args: &[&str], success: bool) -> Result<Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_bro"))
        .current_dir(root)
        .env("HOME", root)
        .env("CODEX_HOME", root.join(".codex"))
        .env("BITROUTER_HOME", root.join(".bitrouter"))
        .arg("--json")
        .args(args)
        .output()
        .await?;
    ensure!(
        output.status.success() == success,
        "status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    if success {
        Ok(serde_json::from_slice(&output.stdout)?)
    } else {
        Ok(serde_json::json!({"stderr":String::from_utf8_lossy(&output.stderr)}))
    }
}
#[tokio::test]
async fn external_directory_is_discovered_read_and_atomically_written_without_a_version_store()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let skills = root.join(".agents/skills");
    source(&skills, "review")?;
    let list = run(root, &["skills", "list"], true).await?;
    ensure!(list["skills"][0]["name"] == "review");
    ensure!(list["skills"][0]["directory_id"].as_str().is_some());
    let read = run(root, &["skills", "read", "review"], true).await?;
    ensure!(
        read["content"]
            .as_str()
            .context("content")?
            .contains("ORIGINAL")
    );
    let input = root.join("input.md");
    std::fs::write(&input, body("review", "UPDATED"))?;
    run(
        root,
        &[
            "skills",
            "write",
            "review",
            "--from",
            input.to_str().context("input path")?,
        ],
        true,
    )
    .await?;
    ensure!(std::fs::read_to_string(skills.join("review/SKILL.md"))?.contains("UPDATED"));
    let support = run(
        root,
        &["skills", "read", "review", "--file", "references/check.md"],
        true,
    )
    .await?;
    ensure!(support["content"] == "support");
    ensure!(!root.join(".bitrouter/skills").exists());
    ensure!(!skills.join("installed.json").exists());
    Ok(())
}
#[tokio::test]
async fn duplicate_names_require_an_explicit_root_and_invalid_edits_keep_the_original() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    source(&root.join(".agents/skills"), "review")?;
    source(&root.join(".claude/skills"), "review")?;
    run(root, &["skills", "read", "review"], false).await?;
    let selected = root.join(".agents/skills");
    let path = selected.join("review/SKILL.md");
    let before = std::fs::read(&path)?;
    run(
        root,
        &[
            "skills",
            "read",
            "review",
            "--root",
            selected.to_str().context("root")?,
        ],
        true,
    )
    .await?;
    let invalid = root.join("invalid.md");
    std::fs::write(&invalid, body("another-name", "INVALID"))?;
    run(
        root,
        &[
            "skills",
            "write",
            "review",
            "--root",
            selected.to_str().context("root")?,
            "--from",
            invalid.to_str().context("input")?,
        ],
        false,
    )
    .await?;
    ensure!(std::fs::read(&path)? == before);
    run(
        root,
        &[
            "skills",
            "read",
            "review",
            "--root",
            selected.to_str().context("root")?,
            "--file",
            "../../outside",
        ],
        false,
    )
    .await?;
    Ok(())
}
#[tokio::test]
async fn installer_commands_are_absent_and_source_files_are_left_untouched() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    source(&root.join("skills"), "review")?;
    let path = root.join("skills/review/SKILL.md");
    let before = std::fs::read(&path)?;
    for action in ["install", "update", "uninstall"] {
        run(root, &["skills", action, "review"], false).await?;
    }
    ensure!(std::fs::read(path)? == before);
    ensure!(!root.join(".bitrouter").exists());
    Ok(())
}
#[cfg(unix)]
#[tokio::test]
async fn supporting_file_symlinks_cannot_read_or_overwrite_another_directory() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    source(&root.join("skills"), "review")?;
    let outside = root.join("outside");
    std::fs::write(&outside, "PRIVATE")?;
    std::os::unix::fs::symlink(&outside, root.join("skills/review/references/link.md"))?;
    run(
        root,
        &["skills", "read", "review", "--file", "references/link.md"],
        false,
    )
    .await?;
    let input = root.join("input");
    std::fs::write(&input, "REPLACED")?;
    run(
        root,
        &[
            "skills",
            "write",
            "review",
            "--file",
            "references/link.md",
            "--from",
            input.to_str().context("input")?,
        ],
        false,
    )
    .await?;
    ensure!(std::fs::read_to_string(outside)? == "PRIVATE");
    Ok(())
}
