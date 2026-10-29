//! Build a Cargo example once per test process, including when running only lib tests.

use std::path::PathBuf;
use std::process::Command;

#[path = "prebuilt_example.rs"]
mod prebuilt_example;

pub fn build(package: &str, example: &str) -> Result<PathBuf, String> {
    if let Some(path) = prebuilt_example::from_env(example)? {
        return Ok(path);
    }
    eprintln!(
        "building Cargo fixture {example}; CI can set BITROUTER_TEST_EXAMPLES_DIR to prebuilt examples"
    );
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
            package,
            "--example",
            example,
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
            "building {example}: {}\n{}",
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
            && artifact["target"]["name"] == example
            && let Some(path) = artifact["executable"].as_str()
        {
            return Ok(PathBuf::from(path));
        }
    }
    Err(format!("Cargo did not return the {example} executable"))
}
