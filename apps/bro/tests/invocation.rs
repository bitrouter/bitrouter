//! Headless process contracts on the independently installed BRO executable.
#[path = "support/binary.rs"]
mod binary;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::process::Command;

#[path = "support/acp.rs"]
mod acp_fixture;

#[test]
fn canonical_help_and_structured_identity_without_service_configuration() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("bitrouter.yaml"), "invalid: [")?;
    for (args, usage) in [
        (&["--help"][..], "Usage: bro"),
        (&["thread", "start", "--help"], "bro thread start"),
        (&["acp", "prompt", "--help"], "bro acp prompt"),
    ] {
        let output = Command::new(binary::bro())
            .args(args)
            .current_dir(dir.path())
            .env("BRO_HOME", dir.path().join("store"))
            .output()?;
        ensure!(output.status.success() && output.stderr.is_empty());
        ensure!(std::str::from_utf8(&output.stdout)?.contains(usage));
    }
    let output = Command::new(binary::bro())
        .arg("build-info")
        .current_dir(dir.path())
        .env("BRO_HOME", dir.path().join("store"))
        .output()?;
    let info: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(output.status.success() && output.stderr.is_empty());
    ensure!(info["product"] == "bitrouter-bro" && info["version"] == env!("CARGO_PKG_VERSION"));
    ensure!(!dir.path().join("store").exists());
    Ok(())
}

#[tokio::test]
async fn piped_acp_prompt_delivers_the_adapter_reply_without_terminal_controls() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let runner = dir
        .path()
        .join(format!("npx{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(acp_fixture::fixture()?, runner)?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new(binary::bro())
            .args(["acp", "prompt", "--agent", "codex", "hello"])
            .current_dir(dir.path())
            .env("BRO_HOME", dir.path().join("bro"))
            .env("PATH", dir.path())
            .env("BITROUTER_ENDPOINT", "http://127.0.0.1:1")
            .env("BITROUTER_API_KEY", "brk_synthetic-acp-bound-token")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("BRO ACP prompt timed out")??;
    ensure!(
        output.status.success(),
        "BRO failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        !output.stdout.contains(&0x1b),
        "redirected BRO emitted terminal controls"
    );
    let lines = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(lines.len() == 2 && lines[0]["type"] == "message_chunk");
    ensure!(lines[0]["text"] == "adapter reply [REDACTED]");
    ensure!(lines[1] == json!({"type":"result","stop_reason":"end_turn"}));
    ensure!(std::fs::read_to_string(dir.path().join("prompt-received"))? == "once");
    Ok(())
}

#[test]
fn renamed_bro_cannot_claim_the_official_receipt() -> Result<()> {
    let home = tempfile::tempdir()?;
    let name = format!("bro{}", std::env::consts::EXE_SUFFIX);
    let official = home.path().join(&name);
    std::fs::copy(binary::bro(), &official)?;
    let renamed = home
        .path()
        .join(format!("renamed{}", std::env::consts::EXE_SUFFIX));
    std::fs::hard_link(&official, &renamed)?;
    let receipts = home.path().join("config");
    let product = receipts.join("bitrouter-bro");
    std::fs::create_dir_all(&product)?;
    let record = json!({
        "source":{"owner":"bitrouter","name":"bitrouter","app_name":"bitrouter-bro","release_type":"github"},
        "provider":{"source":"cargo-dist"},"binaries":[name],"binary_aliases":{},
        "version":env!("CARGO_PKG_VERSION"),"binary_sha256":hex::encode(Sha256::digest(std::fs::read(&official)?)),"install_prefix":home.path(),"install_layout":"flat"
    });
    std::fs::write(
        product.join("bitrouter-bro-receipt.json"),
        serde_json::to_vec(&record)?,
    )?;
    let output = Command::new(renamed)
        .args(["update", "--check"])
        .env("XDG_CONFIG_HOME", &receipts)
        .env("LOCALAPPDATA", &receipts)
        .env("BRO_HOME", home.path().join("store"))
        .output()?;
    ensure!(!output.status.success());
    ensure!(
        std::str::from_utf8(&output.stderr)?.contains("receipt prefix or executable name mismatch"),
        "unexpected receipt refusal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(!home.path().join("store").exists());
    Ok(())
}
