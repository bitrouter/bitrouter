//! Build the requested protocol fixture without compiling the app/runtime.

use std::path::PathBuf;
use std::process::Command;

pub fn build(fixture: &str) -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let profile = executable
        .parent()
        .and_then(|deps| deps.parent())
        .ok_or("test executable has no Cargo profile directory")?;
    let target = profile.parent().ok_or("Cargo target directory missing")?;
    let profile_name = profile
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Cargo profile name missing")?;
    let mut command = Command::new(env!("CARGO"));
    command
        .args([
            "build",
            "--offline",
            "--all-features",
            "--jobs=2",
            "--message-format=json-render-diagnostics",
            "--package",
            "bitrouter-test-fixtures",
            "--bin",
            fixture,
            "--manifest-path",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .arg("--target-dir")
        .arg(target);
    if profile_name != "debug" {
        command.args(["--profile", profile_name]);
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "building {fixture}: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        ));
    }
    // Cargo artifact messages: https://doc.rust-lang.org/cargo/reference/external-tools.html#json-messages
    for line in output.stdout.split(|byte| *byte == b'\n') {
        let Ok(artifact) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if artifact["reason"] == "compiler-artifact"
            && artifact["target"]["name"] == fixture
            && let Some(path) = artifact["executable"].as_str()
        {
            return Ok(PathBuf::from(path));
        }
    }
    Err(format!("Cargo did not return the {fixture} executable"))
}
