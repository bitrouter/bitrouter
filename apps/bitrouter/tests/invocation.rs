//! Process coverage for the service's canonical executable and structured output.

use std::process::{Command, Output};

use anyhow::{Context, Result, ensure};

fn run(args: &[&str], home: &std::path::Path) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args(args)
        .current_dir(home)
        .env("BITROUTER_HOME", home)
        .env("NO_COLOR", "1")
        .output()
        .context("running the gateway CLI")
}

#[test]
fn gateway_help_and_status_use_the_service_identity() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("bitrouter.yaml"), "{}\n")?;
    for (args, expected) in [
        (&["--help"][..], "Usage: bitrouter"),
        (&["serve", "--help"][..], "bitrouter serve"),
        (&["tenancy", "--help"][..], "bitrouter tenancy"),
    ] {
        let output = run(args, home.path())?;
        ensure!(output.status.success(), "help failed for {args:?}");
        let stdout = String::from_utf8(output.stdout)?;
        ensure!(stdout.contains(expected), "unexpected help: {stdout}");
        ensure!(output.stderr.is_empty(), "help emitted a diagnostic");
    }

    let status = run(&["status"], home.path())?;
    ensure!(status.status.success(), "gateway status failed");
    let report: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    ensure!(
        report["running"] == false,
        "unexpected stopped status: {report}"
    );
    Ok(())
}

#[test]
fn gateway_update_delegates_for_the_installed_profile_and_channel() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("bitrouter.yaml"), "{}\n")?;
    let info = run(&["build-info"], home.path())?;
    ensure!(info.status.success());
    let mut identity: serde_json::Value = serde_json::from_slice(&info.stdout)?;
    let product = identity
        .as_object_mut()
        .context("build-info object")?
        .remove("product");
    ensure!(product.as_ref().and_then(serde_json::Value::as_str) == Some("bitrouter"));
    let full = identity["profile"] == "full";
    let mut features = Vec::new();
    for name in identity["features"].as_array().context("feature list")? {
        features.push(name.as_str().context("feature name")?);
    }
    for (method, prefix) in [
        ("homebrew", "Cellar/bin"),
        ("npm", "node_modules/.bin_real"),
        ("cargo", "cargo/bin"),
        ("unknown", "bin"),
    ] {
        let prefix = home.path().join(prefix);
        std::fs::create_dir_all(&prefix)?;
        let exe = prefix.join(format!("bitrouter{}", std::env::consts::EXE_SUFFIX));
        if std::fs::hard_link(env!("CARGO_BIN_EXE_bitrouter"), &exe).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_bitrouter"), &exe)?;
        }
        for flags in [&[][..], &["--stable"], &["--tag", "v1.0.0-beta.1"]] {
            let out = Command::new(&exe)
                .arg("update")
                .args(flags)
                .current_dir(home.path())
                .env("BITROUTER_HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path())
                .env("LOCALAPPDATA", home.path())
                .env("CARGO_HOME", home.path().join("cargo"))
                .output()?;
            ensure!(
                out.status.success(),
                "{method}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let report: serde_json::Value = serde_json::from_slice(&out.stdout)?;
            ensure!(
                report["status"] == "delegated" && report["install_method"] == method,
                "{report}"
            );
            ensure!(report["current_artifact"] == identity);
            let command = report["upgrade_command"]
                .as_str()
                .context("delegated command")?;
            if full {
                ensure!(command.contains("bitrouter"));
            } else {
                ensure!(command.contains(
                    "cargo install bitrouter --bin bitrouter --force --no-default-features"
                ));
                ensure!(command.contains("--features") != features.is_empty());
                ensure!(command.contains(&features.join(",")));
                ensure!(command.contains("--version") == flags.contains(&"--tag"));
                if flags.contains(&"--tag") {
                    ensure!(command.contains("=1.0.0-beta.1"));
                }
            }
        }
    }
    Ok(())
}
