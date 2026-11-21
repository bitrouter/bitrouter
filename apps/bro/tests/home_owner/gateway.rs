use super::binary;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::os::unix::fs::{MetadataExt, symlink};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

fn snapshot(
    root: &Path,
    path: &Path,
    entries: &mut BTreeMap<PathBuf, (u64, u32, Vec<u8>)>,
) -> Result<()> {
    let meta = path.symlink_metadata()?;
    let mut bytes = Vec::new();
    if meta.is_file() {
        bytes = fs::read(path)?;
    } else if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            snapshot(root, &entry?.path(), entries)?;
        }
    } else {
        anyhow::bail!("unexpected fixture entry {}", path.display())
    }
    entries.insert(
        path.strip_prefix(root)?.to_owned(),
        (meta.ino(), meta.mode(), bytes),
    );
    Ok(())
}

pub(super) async fn refuses(gateway: &OsStr, home: &Path, work: &Path) -> Result<()> {
    let config = home.join("new-gateway.yaml");
    let nested = home.join("threads/new-directory/config.yaml");
    let url = format!("sqlite://{}/foreign.db?mode=rwc", home.display());
    let neutral = tempfile::tempdir_in(work)?;
    let neutral = neutral.path();
    let alias = neutral.join("alias");
    symlink(home, &alias)?;
    let aliased_url = "sqlite://alias/foreign.db?mode=rwc";
    let foreign_db_config = neutral.join("foreign-db.yaml");
    fs::write(
        &foreign_db_config,
        format!(
            "inherit_defaults: false\nregistry: {{ enabled: false }}\nserver: {{ listen: '127.0.0.1:0', skip_auth: true }}\ndatabase: {{ url: '{url}' }}\n"
        ),
    )?;
    let mut before = BTreeMap::new();
    snapshot(home, home, &mut before)?;
    for (args, selected) in [
        (
            vec![
                "init",
                "--yes",
                "--after",
                "exit",
                "--force",
                "--provider",
                "openai",
                "--provider-api-key",
                "test-only",
            ],
            home,
        ),
        (vec!["serve"], home),
        (
            vec![
                "init",
                "--yes",
                "--after",
                "exit",
                "-c",
                nested.to_str().context("nested path")?,
            ],
            neutral,
        ),
        (vec!["serve", "-c", "foreign-db.yaml"], neutral),
        (vec!["key", "sign", "--user", "foreign"], home),
        (
            vec![
                "init",
                "--yes",
                "--after",
                "exit",
                "-c",
                config.to_str().context("config path")?,
            ],
            neutral,
        ),
        (
            vec!["serve", "-c", config.to_str().context("config path")?],
            neutral,
        ),
        (
            vec!["key", "sign", "--user", "foreign", "--db", &url],
            neutral,
        ),
        (
            vec!["key", "sign", "--user", "foreign", "--db", aliased_url],
            neutral,
        ),
    ] {
        let mut command = tokio::process::Command::new(gateway);
        command
            .current_dir(neutral)
            .env_clear()
            .env("BITROUTER_HOME", selected)
            .env("HOME", neutral)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_CACHE_HOME", neutral.join("cache"))
            .args(&args)
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(10), command.output()).await??;
        let report = format!(
            "{} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        ensure!(
            !out.status.success() && report.contains("BRO home"),
            "{args:?}: {report}"
        );
        let mut after = BTreeMap::new();
        snapshot(home, home, &mut after)?;
        ensure!(before == after, "{args:?} changed the BRO home");
    }
    Ok(())
}

pub(super) async fn ordinary_and_unmarked(gateway: &OsStr, fixture: &Path) -> Result<()> {
    let home = fixture.join("ordinary");
    let work = fixture.join("tools");
    fs::create_dir(&work)?;
    let run = |args: &[&str]| -> Result<Value> {
        let out = Command::new(binary::bro())
            .current_dir(&work)
            .env("BRO_HOME", &home)
            .args(args)
            .output()?;
        ensure!(
            out.status.success(),
            "BRO {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(serde_json::from_slice(&out.stdout)?)
    };
    run(&["thread", "list"])?;
    refuses(gateway, &home, fixture).await?;
    let thread = run(&["thread", "create"])?["thread_id"]
        .as_str()
        .context("new thread ID")?
        .to_owned();
    let marker = home.join(".bitrouter-home-role");
    ensure!(
        fs::read(&marker)? == b"bro\n",
        "ordinary home missing owner marker"
    );
    for unmarked in [false, true] {
        if unmarked {
            fs::remove_file(&marker)?;
        }
        refuses(gateway, &home, fixture).await?;
        ensure!(run(&["thread", "show", &thread])?["thread_id"] == thread);
        ensure!(marker.is_file(), "reopened BRO home lacks owner marker");
    }
    Ok(())
}
