use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub(crate) fn fixture() -> Result<PathBuf> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT
        .get_or_init(|| build_fixture().map_err(|e| e.to_string()))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn build_fixture() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("BITROUTER_TEST_EXAMPLES_DIR") {
        return Ok(PathBuf::from(dir)
            .join(format!("bro_acp_fixture{}", std::env::consts::EXE_SUFFIX))
            .canonicalize()?);
    }
    ensure!(
        std::env::var_os("BITROUTER_TEST_PREBUILT_ONLY").is_none(),
        "prebuilt ACP fixture directory required"
    );
    let exe = std::env::current_exe()?;
    let profile = exe
        .parent()
        .and_then(Path::parent)
        .context("Cargo test profile missing")?;
    let mut build = std::process::Command::new(env!("CARGO"));
    build
        .args([
            "build",
            "--locked",
            "--offline",
            "--example",
            "bro_acp_fixture",
            "--manifest-path",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .arg("--target-dir")
        .arg(profile.parent().context("Cargo target missing")?);
    let name = profile.file_name().context("Cargo profile missing")?;
    if name != "debug" {
        build.arg("--profile").arg(name);
    }
    ensure!(
        build.status()?.success(),
        "building BRO ACP subprocess fixture failed"
    );
    Ok(profile
        .join("examples")
        .join(format!("bro_acp_fixture{}", std::env::consts::EXE_SUFFIX)))
}
