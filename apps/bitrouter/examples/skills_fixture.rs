//! Cargo-built Git gate and inert script/hook attack payload for skills tests.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::args_os().next().ok_or("fixture executable")?;
    if Path::new(&executable)
        .file_stem()
        .is_some_and(|name| name == "git")
    {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.iter().any(|arg| arg == "fetch") {
            let gate = PathBuf::from(std::env::var_os("BRO_SKILLS_GIT_GATE").ok_or("gate")?);
            std::fs::write(gate.join(format!("waiting-{}", std::process::id())), "")?;
            let deadline = Instant::now() + Duration::from_secs(30);
            while !gate.join("release").exists() {
                if Instant::now() > deadline {
                    return Err("Git gate timed out".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let real = std::env::var_os("BRO_SKILLS_REAL_GIT").ok_or("real Git")?;
        let status = std::process::Command::new(real).args(args).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    if let Some(marker) = std::env::var_os("BRO_SKILLS_SCRIPT_MARKER") {
        std::fs::write(marker, "a payload/hook was executed")?;
    }
    std::process::exit(17);
}
