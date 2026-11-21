//! Retired historical upgrade/import commands fail before opening local data.

use std::process::Command;

use anyhow::{Context, Result, ensure};

#[test]
fn historical_upgrade_and_eval_import_fail_before_touching_files() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    let sentinel = home.join("authority.sqlite");
    let original = b"old authority evidence must remain byte-for-byte unchanged";
    std::fs::write(&sentinel, original)?;
    for args in [
        vec![
            "upgrade-capacity",
            "--database",
            "authority.sqlite",
            "--expected-installation-id",
            "old-installation",
            "--expected-authority-revision",
            "1",
            "--operation-id",
            "upgrade-1",
            "--acknowledge-legacy-count-saturation",
        ],
        vec![
            "eval-inventory",
            "--bundle",
            "authority.sqlite",
            "--evidence-digest",
            "digest",
            "--source-kind",
            "local_user",
            "--report",
            "report",
        ],
        vec![
            "eval-backfill",
            "--plan",
            "authority.sqlite",
            "--plan-digest",
            "digest",
            "--report",
            "report",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bro"))
            .current_dir(home)
            .env("BITROUTER_HOME", home)
            .args(["tenancy", "offline"])
            .args(&args)
            .output()
            .with_context(|| format!("running retired offline command {args:?}"))?;
        ensure!(
            output.status.code() == Some(2),
            "retired command must fail parsing before execution: {args:?}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            std::fs::read(&sentinel)? == original,
            "retired command changed source evidence"
        );
        let files = std::fs::read_dir(home)?.collect::<std::io::Result<Vec<_>>>()?;
        ensure!(
            files.len() == 1 && files.first().is_some_and(|file| file.path() == sentinel),
            "retired command created storage or report artifacts"
        );
    }
    Ok(())
}
