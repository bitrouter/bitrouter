//! A thread retains the user-selected instructions across a source edit and restart.
#[path = "support/gateway.rs"]
mod gateway;

use anyhow::{Context, Result, ensure};
use gateway::{Mock, chunk, stream};
use serde_json::{Value, json};
use std::{path::Path, process::Command};

fn bro(dir: &Path, endpoint: &str, args: &[&str]) -> Result<Vec<Value>> {
    let output = Command::new(env!("CARGO_BIN_EXE_bro"))
        .current_dir(dir)
        .env(
            "BRO_HOME",
            dir.parent().context("fixture parent")?.join("bro-home"),
        )
        .env("BITROUTER_ENDPOINT", endpoint)
        .env("BITROUTER_API_KEY", "scoped-test-key")
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<_>, _>>()?)
}

#[test]
fn activated_skill_is_frozen_before_the_first_paid_request() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let root = workspace.as_path();
    let dir = root.join("skills/review");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    let body = "---\nname: review\ndescription: Review one change.\n---\noriginal-review-material";
    std::fs::write(&path, body)?;
    let mock = Mock::start(vec![stream(format!(
        "{}{}data: [DONE]\n\n",
        chunk(json!({"role":"assistant","content":"ok"}), None),
        chunk(json!({}), Some("stop"))
    ))])?;
    let thread = bro(
        root,
        &mock.endpoint,
        &["thread", "create", "--skill", "review", "--read-only"],
    )?;
    let id = thread[0]["thread_id"].as_str().context("thread")?;
    std::fs::write(
        &path,
        body.replace("original-review-material", "changed-on-disk"),
    )?;
    let events = bro(
        root,
        &mock.endpoint,
        &["thread", "start", id, "review this"],
    )?;
    let turn = events
        .iter()
        .find_map(|e| e["turn_id"].as_str())
        .context("turn")?;
    let explained = bro(
        root,
        &mock.endpoint,
        &["skills", "explain", "--thread", id, "--turn", turn],
    )?;
    ensure!(explained[0]["skills"][0]["name"] == "review");
    let requests = mock.join()?;
    let messages = requests[0]["messages"].as_array().context("messages")?;
    ensure!(messages.iter().any(|m| {
        m["role"] == "user"
            && m["content"]
                .as_str()
                .is_some_and(|c| c.contains("original-review-material"))
    }));
    ensure!(!requests[0].to_string().contains("changed-on-disk"));
    Ok(())
}
